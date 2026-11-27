//! One overlay MCP server behind the tool gateway: a stdio child process spoken to with newline-delimited
//! JSON-RPC. Its env comes from `mcp.json` and goes only to the child; nothing of it is logged.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use harness_agent_config::McpServer;
use serde_json::{json, Value};

const STARTUP: Duration = Duration::from_secs(60);
const CALL: Duration = Duration::from_secs(600);
/// Most `tools/list` pages read from one server.
const MAX_PAGES: usize = 50;

pub(super) struct Upstream {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: u64,
    call_timeout: Duration,
    pub(super) tools: Vec<Value>,
}

fn secs(value: Option<f64>, fallback: Duration) -> Duration {
    value
        .filter(|s| s.is_finite() && *s > 0.0)
        .map_or(fallback, Duration::from_secs_f64)
}

impl Upstream {
    /// Starts `server`, runs the MCP handshake and reads its tool list.
    pub(super) fn start(server: &McpServer) -> Result<Self, String> {
        let command = server
            .command
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .ok_or("no command configured")?;
        let mut cmd = Command::new(resolve(command));
        cmd.args(&server.args)
            .envs(&server.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW：避免每个 MCP 子进程弹出控制台窗口。
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {command}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(message) = serde_json::from_str::<Value>(line.trim()) {
                    if tx.send(message).is_err() {
                        break;
                    }
                }
            }
        });
        let mut up = Self {
            child,
            stdin,
            rx,
            next_id: 0,
            call_timeout: secs(server.tool_timeout_sec, CALL),
            tools: Vec::new(),
        };
        let startup = secs(server.startup_timeout_sec, STARTUP);
        up.request(
            "initialize",
            &json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "harness-tools", "version": env!("CARGO_PKG_VERSION")}
            }),
            startup,
        )?;
        up.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        let mut cursor: Option<Value> = None;
        for _ in 0..MAX_PAGES {
            let params = cursor.map_or_else(|| json!({}), |c| json!({ "cursor": c }));
            let page = up.request("tools/list", &params, startup)?;
            if let Some(tools) = page.get("tools").and_then(Value::as_array) {
                up.tools.extend(tools.iter().cloned());
            }
            cursor = page.get("nextCursor").filter(|c| !c.is_null()).cloned();
            if cursor.is_none() {
                break;
            }
        }
        Ok(up)
    }

    /// `tools/call`; returns the server's result unchanged.
    pub(super) fn call(&mut self, tool: &str, arguments: &Value) -> Result<Value, String> {
        let timeout = self.call_timeout;
        self.request(
            "tools/call",
            &json!({"name": tool, "arguments": arguments}),
            timeout,
        )
    }

    pub(super) fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("server closed its input: {e}"))
    }

    fn request(
        &mut self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = match self.rx.recv_timeout(left) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("{method} timed out after {}s", timeout.as_secs()))
                }
                Err(RecvTimeoutError::Disconnected) => return Err("server exited".into()),
            };
            if message.get("method").is_some() {
                // 上游发来的请求（如 roots/list）网关不支持：回错误，避免对方一直等待。
                if let Some(request_id) = message.get("id") {
                    self.send(&json!({"jsonrpc": "2.0", "id": request_id,
                        "error": {"code": -32601, "message": "not supported by harness-tools"}}))?;
                }
                continue;
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                return Err(text.to_owned());
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `npx` → `npx.cmd` on Windows: `Command` does not apply `PATHEXT`.
fn resolve(command: &str) -> PathBuf {
    let path = Path::new(command);
    if !cfg!(windows) || path.extension().is_some() || path.components().count() > 1 {
        return path.to_path_buf();
    }
    let dirs = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&dirs) {
        for ext in ["exe", "cmd", "bat", "com"] {
            let candidate = dir.join(format!("{command}.{ext}"));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    path.to_path_buf()
}
