//! Session state, persistence and the tool loop of one prompt turn.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::compress::Folds;
use crate::compress_tools;
use crate::llm;
use crate::mcp::McpSet;
use crate::project_docs;
use crate::tools::{self, Ctx};
use crate::{Agent, Reply};

mod prompt;

pub use prompt::prompt_content;
pub(crate) use prompt::request_messages;
use prompt::system_prompt;

/// Empty answers (no text, no tool call) asked again before the turn fails.
const EMPTY_RETRIES: u32 = 2;
/// Sent after a response was cut off at `max_tokens`.
const CUT_OFF_NOTE: &str = "[Harness] Your last response hit the output limit and was cut off; any tool call in it was dropped. Continue the task: keep your reasoning short and split large edits into several smaller edit_file calls.";
/// Images kept in the history; older ones are replaced by a short note.
const KEEP_IMAGES: usize = 4;

pub struct Session {
    pub cwd: String,
    /// Chat messages without the system message, folded ones included.
    pub messages: Vec<Value>,
    /// Folded ranges of `messages`; the model sees their summaries instead.
    pub folds: Folds,
    pub mode: String,
    mcp_specs: Vec<Value>,
    mcp: Option<McpSet>,
    always: HashSet<String>,
}

fn session_file(state: &Path, id: &str) -> Option<PathBuf> {
    // 会话 id 来自客户端：只允许安全字符，防止路径穿越。
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(state.join("sessions").join(format!("{id}.json")))
}

impl Session {
    pub fn new(cwd: String, mcp_specs: Vec<Value>) -> Self {
        Self {
            cwd,
            messages: Vec::new(),
            folds: Folds::default(),
            mode: "default".into(),
            mcp_specs,
            mcp: None,
            always: HashSet::new(),
        }
    }

    /// New MCP servers from `session/load`: restart them on the next prompt.
    pub fn set_mcp(&mut self, specs: Vec<Value>) {
        if specs != self.mcp_specs {
            self.mcp_specs = specs;
            self.mcp = None;
        }
    }

    pub async fn persist(&self, state: Option<&Path>, id: &str) {
        let Some(path) = state.and_then(|s| session_file(s, id)) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        let body = json!({"cwd": self.cwd, "mode": self.mode, "messages": self.messages, "context": self.folds.to_json()});
        let tmp = path.with_extension("json.tmp");
        if tokio::fs::write(&tmp, body.to_string()).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &path).await;
        }
    }

    pub async fn restore(state: Option<&Path>, id: &str, mcp_specs: Vec<Value>) -> Option<Self> {
        let path = session_file(state?, id)?;
        let text = tokio::fs::read_to_string(path).await.ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let mut session = Self::new(
            value.get("cwd").and_then(Value::as_str)?.to_owned(),
            mcp_specs,
        );
        value
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .clone_into(&mut session.mode);
        session.messages = value
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        session.folds = Folds::from_json(value.get("context"));
        Some(session)
    }

    /// `(is_user, text)` of the visible conversation, for `session/load` replay.
    pub fn transcript(&self) -> Vec<(bool, String)> {
        self.messages
            .iter()
            .filter_map(|m| {
                let role = m.get("role").and_then(Value::as_str)?;
                let mut text = llm::content_text(m.get("content"))?;
                if let Some(at) = text.find("\n\n[Harness context]") {
                    text.truncate(at);
                }
                match role {
                    "user" => Some((true, text)),
                    "assistant" if !text.is_empty() => Some((false, text)),
                    _ => None,
                }
            })
            .collect()
    }

    /// Folds (then, as a last resort, drops) the oldest messages until the visible history fits the budget.
    fn trim(&mut self) {
        self.folds.fit(&mut self.messages);
    }
}

/// One prompt turn: model call → tools → model call … until the model answers without tools.
#[allow(clippy::too_many_lines)]
pub async fn run_turn(
    agent: &Agent,
    id: &str,
    session: &mut Session,
    user: Value,
    token: &CancellationToken,
) -> Reply {
    if agent.provider.base_url.is_empty() || agent.provider.model.is_empty() {
        return Err(
            json!({"code": -32603, "message": "provider endpoint or model is not configured"}),
        );
    }
    if session.mcp.is_none() {
        let set = McpSet::new(&session.mcp_specs);
        set.start_eager().await;
        session.mcp = Some(set);
    }
    let user = if user.as_str().is_some_and(|t| t.trim() == "/compact") {
        json!(compress_tools::COMPACT_PROMPT)
    } else {
        user
    };
    session
        .messages
        .push(json!({"role": "user", "content": user}));
    llm::prune_images(&mut session.messages, KEEP_IMAGES);
    session.trim();
    if let Some(note) = compress_tools::nudge(&mut session.folds, &session.messages) {
        if let Some(last) = session.messages.last_mut() {
            compress_tools::append_note(last, &note);
        }
    }
    let mut input_tokens = 0;
    let mut output_tokens = 0;
    let mut cached_read = 0;
    let mut cached_write = 0;
    let mut empty_answers = 0;
    // No cap on model calls per turn: the turn runs until the model answers without tools or is cancelled.
    // Project instructions (AGENTS.md / CLAUDE.md) are read once for the whole turn: the system prompt
    // must stay byte-identical across the model requests of a turn for prompt caching to hit.
    let instructions = project_docs::instructions(&session.cwd);
    // Decided once per turn so every request of the turn has the same prefix (prompt caching).
    let system_as_user = agent.system_as_user().await;
    loop {
        let mut tool_defs = tools::definitions(&session.mode, agent.async_tasks());
        if let Some(mcp) = &session.mcp {
            tool_defs.extend(mcp.definitions().await);
        }
        if crate::code_mode::enabled() {
            let definition = crate::code_mode::definition(&tool_defs);
            tool_defs.push(definition);
        }
        tool_defs.extend(compress_tools::definitions());
        session.trim();
        let messages = request_messages(
            &system_prompt(agent, session, &instructions),
            &session.folds.view(&session.messages),
            system_as_user,
        );
        let completion = llm::complete(agent, id, &messages, &tool_defs, token).await?;
        input_tokens += completion.input_tokens;
        output_tokens += completion.output_tokens;
        cached_read += completion.cache_read_tokens;
        cached_write += completion.cache_write_tokens;
        // 修复：API agent 从不报 usage_update，切到自定义 provider 后上下文面板一直显示 Claude 的旧数字。
        let request_chars = messages.iter().map(llm::message_size).sum::<usize>()
            + serde_json::to_string(&tool_defs).map_or(0, |s| s.len());
        agent.update(
            id,
            &crate::context_meter::update(
                completion.input_tokens,
                completion.output_tokens,
                request_chars,
                session.folds.view_size(&session.messages),
            ),
        );
        if !completion.cancelled && completion.finish_reason.as_deref() == Some("length") {
            // Cut off at the output limit: tool calls may be truncated JSON, so drop them, keep any text and
            // ask the model to continue in smaller steps instead of ending the turn silently.
            if !completion.content.trim().is_empty() {
                session
                    .messages
                    .push(json!({"role": "assistant", "content": completion.content}));
            }
            session
                .messages
                .push(json!({"role": "user", "content": CUT_OFF_NOTE}));
            continue;
        }
        if !completion.cancelled
            && completion.tool_calls.is_empty()
            && completion.content.trim().is_empty()
        {
            // An empty answer is a provider glitch, not the end of the task: ask again.
            empty_answers += 1;
            if empty_answers > EMPTY_RETRIES {
                return Err(llm::transient(
                    &format!(
                        "{}: the model returned an empty response",
                        agent.provider.label
                    ),
                    None,
                ));
            }
            continue;
        }
        let mut assistant = json!({"role": "assistant", "content": completion.content});
        // Thinking models (DeepSeek and others) keep their plan across tool calls only when their reasoning
        // is sent back within the turn; without it they re-think from scratch after every tool result.
        if !completion.tool_calls.is_empty()
            && !completion.reasoning.is_empty()
            && agent.echo_reasoning()
        {
            assistant["reasoning_content"] = json!(completion.reasoning);
        }
        if !completion.tool_calls.is_empty() {
            assistant["tool_calls"] = json!(completion
                .tool_calls
                .iter()
                .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": if c.arguments.trim().is_empty() { "{}" } else { c.arguments.as_str() }}}))
                .collect::<Vec<_>>());
        }
        session.messages.push(assistant);
        if completion.cancelled {
            return Ok(json!({"stopReason": "cancelled"}));
        }
        if completion.tool_calls.is_empty() {
            return Ok(
                json!({"stopReason": "end_turn", "usage": usage(input_tokens, output_tokens, cached_read, cached_write)}),
            );
        }
        let cwd = PathBuf::from(&session.cwd);
        let mode = session.mode.clone();
        let empty = McpSet::default();
        let mut always = std::mem::take(&mut session.always);
        let mut results = Vec::new();
        let images = std::sync::Mutex::new(Vec::new());
        {
            let mut ctx = Ctx {
                agent,
                session: id,
                cwd: &cwd,
                mode: &mode,
                always: &mut always,
                token,
                mcp: session.mcp.as_ref().unwrap_or(&empty),
                images: &images,
            };
            for call in &completion.tool_calls {
                let text = if token.is_cancelled() {
                    "Error: cancelled".to_owned()
                } else if compress_tools::handles(&call.name) {
                    compress_tools::run(agent, id, &mut session.folds, &session.messages, call)
                } else {
                    ctx.run(call).await
                };
                results.push(json!({"role": "tool", "tool_call_id": call.id, "content": text}));
            }
        }
        session.always = always;
        session.messages.extend(results);
        if let Some(note) = compress_tools::nudge(&mut session.folds, &session.messages) {
            if let Some(last) = session.messages.last_mut() {
                compress_tools::append_note(last, &note);
            }
        }
        let images = images.into_inner().unwrap_or_default();
        if !images.is_empty() {
            let mut parts = vec![json!({"type": "text", "text": "Images returned by read_file:"})];
            parts.extend(images);
            session
                .messages
                .push(json!({"role": "user", "content": parts}));
            llm::prune_images(&mut session.messages, KEEP_IMAGES);
        }
        if token.is_cancelled() {
            return Ok(json!({"stopReason": "cancelled"}));
        }
    }
}

fn usage(input: u64, output: u64, cached_read: u64, cached_write: u64) -> Value {
    json!({
        "inputTokens": input,
        "outputTokens": output,
        "totalTokens": input + output,
        "cachedReadTokens": cached_read,
        "cachedWriteTokens": cached_write
    })
}

#[cfg(test)]
mod request_messages_tests {
    use super::request_messages;
    use serde_json::json;

    #[test]
    fn system_role_by_default() {
        let history = vec![json!({"role": "user", "content": "hi"})];
        let m = request_messages("be brief", &history, false);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], json!({"role": "system", "content": "be brief"}));
        assert_eq!(m[1]["role"], "user");
    }

    #[test]
    fn folded_into_first_user_message() {
        let history = vec![json!({"role": "user", "content": "hi"})];
        let m = request_messages("be brief", &history, true);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0]["role"], "user");
        assert!(m[0]["content"]
            .as_str()
            .is_some_and(|c| c.contains("be brief")));
        assert_eq!(m[1], json!({"role": "assistant", "content": "Understood."}));
        assert_eq!(m[2]["content"], "hi");
        assert!(m.iter().all(|x| x["role"] != "system"));
    }
}
