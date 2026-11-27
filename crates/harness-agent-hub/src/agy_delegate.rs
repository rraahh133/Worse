//! agy keeps using its own `invoke_subagent` when asked for subagents, even with the `harness-delegate` server
//! instructions loaded, so the hub repeats them in the prompt of agy parent sessions, and when agy still calls
//! `invoke_subagent` the session stops that turn and queues one redirect turn (phase-9-parity.md §9.8.1).

use harness_core_api::{AgentId, SessionId};

use crate::hub::HubCtx;

const HINT: &str = "<harness-subagents>
You run inside Harness. When the user asks you to spawn, use or deploy subagents, delegate work, split work across agents, or ask another agent or model, use the Harness subagent tools of the MCP server harness-delegate:
- list_agents (mcp_harness-delegate_list_agents) to see the user's agents,
- delegate (mcp_harness-delegate_delegate) with a complete `task`, an `agent` id from list_agents and `background: true` to run several subagents in parallel,
- subagent_message (mcp_harness-delegate_subagent_message) to send a running subagent more instructions.
Do not use your built-in invoke_subagent or manage_subagents tools for this: they only run copies of yourself that the user cannot see or manage in Harness, and Harness stops any invoke_subagent call in this session.
Call these tools directly; do not look for them in files, processes, environment variables or the Harness source code. Only when the user asks for subagents or delegation and mcp_harness-delegate_delegate is not among your tools, tell the user in one sentence that the Harness delegate tool is not available in this session and stop; never do the subagents' work yourself instead. Give each subagent different files to edit; results come back to you as new messages.
Every other message: ignore this note, answer the user's message normally and do not mention these tools.
</harness-subagents>";
// 修复：新会话（例如配额切换账号后）每轮都会带上本提示；旧措辞让 agy 在用户只问"完成了吗"时也回答
// "delegate 工具不可用"并停止。现在只有用户要求子代理时才报告缺少工具。

const TRIGGERS: [&str; 12] = [
    "subagent",
    "sub-agent",
    "sub agent",
    "delegate",
    "delegasi",
    "spawn",
    "agent lain",
    "agen lain",
    "other agent",
    "another agent",
    "parallel",
    "paralel",
];

/// Client message id prefix of the follow-up turn the hub queues after stopping agy's built-in spawn.
pub(crate) const REDIRECT_ID_PREFIX: &str = "agy-redirect-";

const PROMPT_CAP: usize = 4000;

/// The subagent instructions for this prompt: agy parent sessions only, on a fresh agent session or when the
/// message talks about subagents.
pub(crate) fn hint(
    ctx: &HubCtx,
    agent: &AgentId,
    session: &SessionId,
    fresh: bool,
    user_text: &str,
) -> Option<&'static str> {
    let parent = ctx.subagents.endpoint.get().is_some()
        && agy_role(ctx, agent, session) == Some(Role::Parent);
    (parent && (fresh || mentions(user_text))).then_some(HINT)
}

/// What an agy session may do instead of its built-in spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Top-level session: has the harness-delegate `delegate` tool.
    Parent,
    /// A Harness subagent: cannot start agents, only `notify_parent`.
    Child,
    /// An inbox router: hands requests on with `route_trigger`.
    Router,
}

/// The role of `session` when its agent is agy; `None` for every other agent.
pub(crate) fn agy_role(ctx: &HubCtx, agent: &AgentId, session: &SessionId) -> Option<Role> {
    if !crate::agy_ops::is_agy(ctx, agent) {
        return None;
    }
    Some(if ctx.routers.is_router(session) {
        Role::Router
    } else if ctx.subagents.is_child(session) && !ctx.config.delegation.nested() {
        Role::Child
    } else {
        Role::Parent
    })
}

/// agy reports its built-in spawn as the tool call `Run invoke_subagent` with the task in `rawInput.Prompt`.
/// Returns that task when `title` is that tool.
pub(crate) fn builtin_spawn(title: &str, raw_input: Option<&str>) -> Option<String> {
    let title = title.trim().to_lowercase();
    if title.strip_prefix("run ").unwrap_or(&title).trim() != "invoke_subagent" {
        return None;
    }
    let input = raw_input
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .unwrap_or_default();
    let prompt = ["Prompt", "prompt", "Task", "task"]
        .iter()
        .find_map(|key| input.get(*key).and_then(serde_json::Value::as_str))
        .unwrap_or_default();
    Some(prompt.chars().take(PROMPT_CAP).collect())
}

/// The follow-up message that tells agy what to do with the task it tried to spawn, for its session role.
pub(crate) fn redirect_text(role: Role, task: &str) -> String {
    let task = if task.trim().is_empty() {
        "the work you were about to hand to a subagent".to_owned()
    } else {
        format!("this task:\n<task>\n{}\n</task>", task.trim())
    };
    let next = match role {
        Role::Parent => format!(
            "In Harness, subagents must be started with the harness-delegate tools so the user can see and manage \
them. Start it again now with mcp_harness-delegate_delegate (background true, agent id from \
mcp_harness-delegate_list_agents) for {task}\nIf mcp_harness-delegate_delegate is not among your tools, tell the \
user in one sentence that the Harness delegate tool is not available in this session and stop."
        ),
        Role::Child => format!(
            "You are a Harness subagent and may not start other agents. Do {task}\nyourself in this turn; if it is \
too big, say so with mcp_harness-delegate_notify_parent and stop."
        ),
        Role::Router => format!(
            "You are a Harness inbox router. Hand the request to an agent with mcp_harness-delegate_route_trigger \
instead of {task}"
        ),
    };
    format!(
        "[Harness] Your built-in invoke_subagent call was stopped. {next}\nDo not call invoke_subagent or \
manage_subagents again."
    )
}

fn mentions(text: &str) -> bool {
    let text = text.to_lowercase();
    TRIGGERS.iter().any(|word| text.contains(word))
}

#[cfg(test)]
mod tests {
    #[test]
    fn subagent_requests_are_recognised() {
        assert!(super::mentions("spawn subagent aja delegate task"));
        assert!(super::mentions("kerjakan PARALEL pakai agent lain"));
        assert!(!super::mentions("fix the login bug"));
        assert!(super::HINT.contains("mcp_harness-delegate_delegate"));
    }

    #[test]
    fn built_in_spawn_is_recognised_and_redirected_with_its_task() {
        let raw = r#"{"Model":"inherit","Prompt":"Build the dashboard"}"#;
        assert_eq!(
            super::builtin_spawn("Run invoke_subagent", Some(raw)).as_deref(),
            Some("Build the dashboard")
        );
        assert_eq!(
            super::builtin_spawn("invoke_subagent", None).as_deref(),
            Some("")
        );
        assert!(super::builtin_spawn("Run mcp_harness-delegate_delegate", Some(raw)).is_none());
        assert!(super::builtin_spawn("Run manage_subagents", None).is_none());
        assert!(super::builtin_spawn("File src/invoke_subagent_test.rs", None).is_none());
        let text = super::redirect_text(super::Role::Parent, "Build the dashboard");
        assert!(text.contains("mcp_harness-delegate_delegate"));
        assert!(text.contains("<task>\nBuild the dashboard\n</task>"));
        let child = super::redirect_text(super::Role::Child, "Build the dashboard");
        assert!(
            child.contains("notify_parent") && !child.contains("mcp_harness-delegate_delegate")
        );
        let router = super::redirect_text(super::Role::Router, "");
        assert!(router.contains("route_trigger"));
    }
}
