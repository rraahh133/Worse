//! Loopback delegate server (phase-9-parity.md §9.8 Transport): one JSON request line per TCP connection from
//! `harness-memory-mcp --delegate`, one JSON reply line.

use std::fmt::Write as _;
use std::sync::{Arc, Weak};

use harness_core_api::{
    AgentId, CreateSession, EventKind, HarnessCore, OrchestraNode, OrchestraRole, QuotaState,
    RecvError, ScopeView, SendMessage, SessionId, SubagentStatus, SubagentView, TurnStatus,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::delegate::{preview, Endpoint};
use crate::hub::Hub;
use crate::store;
use crate::util::now_ms;

mod calls;
mod delegation;
mod push;
mod results;

use calls::{agent_state, agents, claim, message, notify, quota, release, task_text, team, wait};
use delegation::delegate;
pub(crate) use push::push;
pub(crate) use results::new_view;
use results::{delegation_scope, result, set_scope, turn_ended, PREAMBLE};

/// Largest request line (a task is at most 100 000 characters, i.e. up to 400 KB of UTF-8).
const MAX_REQUEST: u64 = 1024 * 1024;
const MAX_TASK_CHARS: usize = 100_000;

/// Binds `127.0.0.1:0` and serves until aborted. Returns the endpoint and the accept task.
pub(crate) async fn start(
    hub: Weak<Hub>,
) -> std::io::Result<(Endpoint, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let endpoint = Endpoint {
        addr,
        token: token.clone(),
    };
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let (hub, token) = (hub.clone(), token.clone());
            tokio::spawn(async move {
                if let Err(error) = connection(hub, &token, stream).await {
                    tracing::debug!(%error, "delegate connection ended");
                }
            });
        }
    });
    Ok((endpoint, task))
}

async fn connection(hub: Weak<Hub>, token: &str, stream: TcpStream) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read.take(MAX_REQUEST));
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let request: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    let reply = match (request["token"].as_str(), hub.upgrade()) {
        (Some(given), Some(hub)) if constant_eq(given, token) => {
            // 读端保留下来：父 agent 取消工具调用时 MCP 进程会断开连接，据此停止子会话（§9.8）。
            let closed = async move {
                let mut sink = [0u8; 64];
                let mut rest = reader.into_inner();
                loop {
                    match rest.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            };
            match handle(&hub, &request, closed).await {
                Ok(value) => json!({"ok": true, "result": value}),
                Err(message) => json!({"ok": false, "error": message}),
            }
        }
        (_, None) => json!({"ok": false, "error": "Harness is shutting down"}),
        _ => json!({"ok": false, "error": "invalid delegate token"}),
    };
    write.write_all(format!("{reply}\n").as_bytes()).await?;
    write.flush().await
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

async fn handle(
    hub: &Arc<Hub>,
    request: &Value,
    closed: impl std::future::Future<Output = ()>,
) -> Result<Value, String> {
    let parent = SessionId::new(request["parent"].as_str().unwrap_or_default());
    let parent_row = {
        let lookup = parent.clone();
        hub.ctx
            .store
            .run(move |c| store::load_session(c, &lookup))
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown parent session {parent}"))?
    };
    // 角色以 hub 注册表为准（§9.8.1），环境变量里的 role 只决定工具列表，不授予权限。
    let is_child = hub.ctx.subagents.is_child(&parent);
    let is_router = hub.ctx.routers.is_router(&parent);
    let args = &request["args"];
    let op = request["op"].as_str();
    if op == Some("delegate")
        && !is_child
        && !crate::orchestra::may_delegate(&hub.ctx, &parent_row.active_agent).await
    {
        return Err(
            "this agent is not connected in the Harness Orchestra, so it cannot start subagents"
                .into(),
        );
    }
    match op {
        Some("route" | "dismiss") if !is_router => {
            Err("only the Harness inbox router can route or dismiss triggers".into())
        }
        Some("route") => hub.route_trigger(&parent, args).await,
        Some("dismiss") => hub.dismiss_trigger(&parent, args).await,
        Some(op) if is_router && op != "agents" => {
            Err("the inbox router can only list agents, route and dismiss triggers".into())
        }
        Some("agents" | "delegate" | "message" | "wait")
            if is_child && !hub.ctx.config.delegation.nested() =>
        {
            Err("a subagent cannot delegate or manage subagents; use notify_parent".into())
        }
        Some("notify") if !is_child => Err("notify_parent is only for subagents".into()),
        Some("agents") => Ok(agents(hub, &parent_row).await),
        Some("delegate") => delegate(hub, &parent_row, args, closed).await,
        Some("message") => message(hub, &parent_row, args).await,
        Some("wait") => wait(hub, &parent_row, args, closed).await,
        Some("notify") => notify(hub, &parent_row, args).await,
        Some("claim") => claim(hub, &parent_row, args),
        Some("release") => release(hub, &parent_row, args),
        other => Err(format!("unknown op {other:?}")),
    }
}
