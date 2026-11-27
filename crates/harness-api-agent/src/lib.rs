//! `harness-api-agent`: Harness's own ACP coding agent for OpenAI-compatible APIs (phase-9-parity.md §9.7).
//!
//! The hub starts one process per provider with `HARNESS_PROVIDER_BASE_URL`, `HARNESS_PROVIDER_MODEL`,
//! `HARNESS_PROVIDER_API_KEY`, `HARNESS_PROVIDER_LABEL` and `HARNESS_PROVIDER_STATE` (session files). It speaks ACP
//! (newline-delimited JSON-RPC) on stdio and calls `POST {base}/chat/completions` with streaming and tools.
//! The API key is only ever put into the `Authorization` header; it is never logged or echoed.

mod background;
mod code_mode;
mod code_mode_engine;
mod code_mode_script;
#[cfg(test)]
mod code_mode_tests;
mod code_mode_types;
mod compress;
mod compress_tools;
mod context_meter;
mod factcheck;
mod factcheck_claims;
mod hermes;
mod llm;
mod mcp;
mod papers;
mod project_docs;
mod prompt_source;
mod search;
mod session;
mod tools;
mod web;
mod web_search;
mod web_text;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::session::Session;

mod handlers;
mod server;

pub use server::run;
use server::text_param;

/// Provider settings from the environment.
pub(crate) struct Provider {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub label: String,
    pub state_dir: Option<std::path::PathBuf>,
    /// `max_tokens` sent with every request; `0` leaves it to the provider (some proxies default to 1024,
    /// which cuts off reasoning and large tool calls).
    pub max_tokens: u64,
    /// Send prompt-cache breakpoints (`HARNESS_PROVIDER_PROMPT_CACHE`, on unless `0`/`false`).
    pub prompt_cache: bool,
    /// Where the system prompt goes (`HARNESS_PROVIDER_SYSTEM_ROLE`: `system`, `user`, or `auto` = probe once).
    pub system_role: SystemRole,
    /// What the endpoint is (`HARNESS_PROVIDER_KIND`: `hermes`, `openai`, or `auto` = ask it once).
    pub kind: Kind,
}

/// What answers at the provider's endpoint: a model API, or a Hermes gateway (a whole agent that runs its
/// own tools; see `hermes.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Auto,
    OpenAi,
    Hermes,
}

/// How the provider receives Harness's system prompt. Some OpenAI-compatible proxies drop or replace the
/// `system` message, so the model never sees the instructions; those get them as the first user message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SystemRole {
    Auto,
    System,
    User,
}

/// Output budget per request unless `HARNESS_PROVIDER_MAX_TOKENS` says otherwise.
const DEFAULT_MAX_TOKENS: u64 = 8192;

impl Provider {
    fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).unwrap_or_default().trim().to_owned();
        let label = var("HARNESS_PROVIDER_LABEL");
        Self {
            base_url: var("HARNESS_PROVIDER_BASE_URL")
                .trim_end_matches('/')
                .to_owned(),
            model: var("HARNESS_PROVIDER_MODEL"),
            api_key: var("HARNESS_PROVIDER_API_KEY"),
            label: if label.is_empty() {
                "API agent".into()
            } else {
                label
            },
            state_dir: std::env::var_os("HARNESS_PROVIDER_STATE")
                .filter(|v| !v.is_empty())
                .map(std::path::PathBuf::from),
            max_tokens: var("HARNESS_PROVIDER_MAX_TOKENS")
                .parse()
                .unwrap_or(DEFAULT_MAX_TOKENS),
            prompt_cache: !matches!(
                var("HARNESS_PROVIDER_PROMPT_CACHE")
                    .to_ascii_lowercase()
                    .as_str(),
                "0" | "false"
            ),
            system_role: match var("HARNESS_PROVIDER_SYSTEM_ROLE")
                .to_ascii_lowercase()
                .as_str()
            {
                "system" => SystemRole::System,
                "user" => SystemRole::User,
                _ => SystemRole::Auto,
            },
            kind: match var("HARNESS_PROVIDER_KIND").to_ascii_lowercase().as_str() {
                "hermes" => Kind::Hermes,
                "openai" => Kind::OpenAi,
                _ => Kind::Auto,
            },
        }
    }
}

pub(crate) type Reply = Result<Value, Value>;

/// The ACP side: stdout writer, client requests in flight, sessions.
pub(crate) struct Agent {
    pub provider: Provider,
    pub prompts: Box<dyn prompt_source::PromptSource>,
    pub http: reqwest::Client,
    out: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: Mutex<u64>,
    sessions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Session>>>>,
    cancels: Mutex<HashMap<String, CancellationToken>>,
    me: Weak<Agent>,
    jobs: background::Jobs,
    async_tasks: AtomicBool,
    /// Send `reasoning_content` back with the assistant messages of the current turn; cleared once a
    /// provider rejects it (HTTP 400).
    echo_reasoning: AtomicBool,
    /// Provider accepts `cache_control` breakpoints; cleared once one rejects them (HTTP 400).
    cache_control: AtomicBool,
    vision: AtomicBool,
    /// Resolved system-prompt placement: 0 = not probed yet, 1 = `system` role, 2 = first user message.
    system_mode: std::sync::atomic::AtomicU8,
    /// Serializes the one-time probe so parallel sessions don't each send one.
    system_probe: tokio::sync::Mutex<()>,
    /// Whether the endpoint is a Hermes gateway: 0 = not asked yet, 1 = no, 2 = yes.
    pub(crate) gateway_mode: std::sync::atomic::AtomicU8,
}

impl Agent {
    fn dispatch(self: &Arc<Self>, message: &Value) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (None, Some(id)) => {
                let sender = id
                    .as_u64()
                    .and_then(|id| self.pending.lock().ok().and_then(|mut p| p.remove(&id)));
                if let Some(sender) = sender {
                    let _ = sender.send(message.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            (Some(method), None) => {
                if method == "session/cancel" {
                    self.cancel(&text_param(&params, "sessionId"));
                }
            }
            (Some(method), Some(id)) => {
                let agent = Arc::clone(self);
                tokio::spawn(async move {
                    let reply = agent.handle(&method, &params).await;
                    agent.reply(&id, reply);
                });
            }
            (None, None) => {}
        }
    }

    pub fn send(&self, value: &Value) {
        let _ = self.out.send(value.to_string());
    }

    fn reply(&self, id: &Value, reply: Reply) {
        match reply {
            Ok(result) => self.send(&json!({"jsonrpc":"2.0","id":id,"result":result})),
            Err(error) => self.send(&json!({"jsonrpc":"2.0","id":id,"error":error})),
        }
    }

    /// ACP `session/update` notification.
    pub fn update(&self, session: &str, update: &Value) {
        self.send(&json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}}));
    }

    /// Agent → client request; `Null` when the client goes away.
    pub async fn request(&self, method: &str, params: Value) -> Value {
        let (tx, rx) = oneshot::channel();
        let id = {
            let Ok(mut next) = self.next_id.lock() else {
                return Value::Null;
            };
            *next += 1;
            *next
        };
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        rx.await.unwrap_or(Value::Null)
    }

    fn cancel(&self, session: &str) {
        if let Ok(map) = self.cancels.lock() {
            if let Some(token) = map.get(session) {
                token.cancel();
            }
        }
        self.jobs.cancel_turns(session);
    }

    fn cancel_all(&self) {
        if let Ok(map) = self.cancels.lock() {
            for token in map.values() {
                token.cancel();
            }
        }
        self.jobs.cancel_all();
    }

    pub fn echo_reasoning(&self) -> bool {
        self.echo_reasoning.load(Ordering::SeqCst)
    }

    pub fn disable_echo_reasoning(&self) {
        self.echo_reasoning.store(false, Ordering::SeqCst);
    }

    pub fn cache_control(&self) -> bool {
        self.cache_control.load(Ordering::SeqCst)
    }

    pub fn disable_cache_control(&self) {
        self.cache_control.store(false, Ordering::SeqCst);
    }

    pub fn vision(&self) -> bool {
        self.vision.load(Ordering::SeqCst)
    }

    pub fn disable_vision(&self) {
        self.vision.store(false, Ordering::SeqCst);
    }

    /// Whether the system prompt must go into the first user message (the provider ignores `system`).
    /// Probes the provider once per process (and remembers the answer in the state dir) when `auto`.
    pub async fn system_as_user(&self) -> bool {
        match self.provider.system_role {
            SystemRole::System => return false,
            SystemRole::User => return true,
            SystemRole::Auto => {}
        }
        let _guard = self.system_probe.lock().await;
        match self.system_mode.load(Ordering::SeqCst) {
            1 => return false,
            2 => return true,
            _ => {}
        }
        let key = format!("{}|{}", self.provider.base_url, self.provider.model);
        let file = self
            .provider
            .state_dir
            .as_ref()
            .map(|d| d.join("system-role.json"));
        let mut honored = None;
        if let Some(file) = &file {
            if let Ok(text) = tokio::fs::read_to_string(file).await {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if v.get("key").and_then(Value::as_str) == Some(key.as_str()) {
                        honored = v.get("systemHonored").and_then(Value::as_bool);
                    }
                }
            }
        }
        if honored.is_none() {
            honored = crate::llm::probe_system_role(self).await;
            if let (Some(h), Some(file)) = (honored, &file) {
                if let Some(dir) = file.parent() {
                    let _ = tokio::fs::create_dir_all(dir).await;
                }
                let _ = tokio::fs::write(file, json!({"key": key, "systemHonored": h}).to_string())
                    .await;
            }
        }
        // Unknown (probe failed): keep the standard `system` role for this request, probe again next turn.
        match honored {
            Some(true) => self.system_mode.store(1, Ordering::SeqCst),
            Some(false) => self.system_mode.store(2, Ordering::SeqCst),
            None => {}
        }
        honored == Some(false)
    }

    pub fn async_tasks(&self) -> bool {
        self.async_tasks.load(Ordering::SeqCst)
    }

    fn note_client(&self, params: &Value) {
        let declared = params
            .pointer("/clientCapabilities/_meta/jetbrains/air/capabilities")
            .and_then(Value::as_array)
            .is_some_and(|caps| caps.iter().any(|c| c.as_str() == Some("asyncTasks")));
        self.async_tasks.store(declared, Ordering::SeqCst);
    }

    fn session(&self, id: &str) -> Option<Arc<tokio::sync::Mutex<Session>>> {
        self.sessions.lock().ok()?.get(id).cloned()
    }
}
