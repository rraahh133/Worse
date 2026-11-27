//! `<exe> --connector-mcp <id>`: a stdio MCP server that forwards every JSON-RPC message to the connector's
//! remote server (streamable HTTP) with the stored token, refreshing it on expiry or a 401. Agents see a plain
//! stdio server; tokens stay in this process.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::catalog::Auth;
use crate::http::{client, Sse};
use crate::state::Connected;
use crate::{oauth, ConnectorError, Result};

struct Bridge {
    home: PathBuf,
    id: String,
    title: String,
    url: String,
    oauth: bool,
    http: reqwest::Client,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    out: Mutex<tokio::io::Stdout>,
}

/// Runs the bridge for `args` = `[<id>]`; data directory from `HARNESS_HOME` (else `~/.harness`).
#[must_use = "return the exit code from main"]
pub fn run(args: impl IntoIterator<Item = String>) -> ExitCode {
    let Some(id) = args.into_iter().next() else {
        eprintln!("harness connector-mcp: missing connector id");
        return ExitCode::FAILURE;
    };
    let home = std::env::var_os("HARNESS_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|h| PathBuf::from(h).join(".harness"))
        });
    let Some(home) = home else {
        eprintln!("harness connector-mcp: no data directory");
        return ExitCode::FAILURE;
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("harness connector-mcp: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve(&home, &id)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness connector-mcp {id}: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(home: &Path, id: &str) -> Result<()> {
    let connected = Connected::load(home).await;
    let connection = connected
        .connectors
        .get(id)
        .ok_or_else(|| ConnectorError::Invalid(format!("{id} is not connected")))?;
    let bridge = Arc::new(Bridge {
        home: home.to_path_buf(),
        id: id.to_owned(),
        title: connection.title.clone(),
        url: connection.url.clone(),
        oauth: connection.auth == Auth::OAuth,
        http: client()?,
        session: Mutex::new(None),
        protocol: Mutex::new(None),
        out: Mutex::new(tokio::io::stdout()),
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let is_request = message.get("id").is_some() && message.get("method").is_some();
        let is_initialize = message.get("method").and_then(Value::as_str) == Some("initialize");
        if is_request && !is_initialize {
            let bridge = Arc::clone(&bridge);
            tasks.spawn(async move { bridge.forward(message).await });
        } else {
            // 通知与 initialize 按顺序转发：initialized 必须先于后续请求到达服务器。
            bridge.forward(message).await;
        }
        while tasks.try_join_next().is_some() {}
    }
    while tasks.join_next().await.is_some() {}
    bridge.close().await;
    Ok(())
}

impl Bridge {
    async fn write(&self, message: &str) {
        let mut out = self.out.lock().await;
        let _ = out.write_all(message.as_bytes()).await;
        let _ = out.write_all(b"\n").await;
        let _ = out.flush().await;
    }

    async fn reply_error(&self, id: Option<&Value>, message: &str) {
        if let Some(id) = id {
            let error =
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}});
            self.write(&error.to_string()).await;
        }
    }

    async fn request(&self, message: &Value, token: Option<&str>) -> Result<reqwest::Response> {
        let mut request = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(message.to_string());
        if let Some(session) = self.session.lock().await.as_deref() {
            request = request.header("mcp-session-id", session);
        }
        if let Some(version) = self.protocol.lock().await.as_deref() {
            request = request.header("mcp-protocol-version", version);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        Ok(request.send().await?)
    }

    async fn token(&self, force: bool) -> Result<Option<String>> {
        if !self.oauth {
            return Ok(None);
        }
        oauth::access_token(&self.home, &self.id, force).await
    }

    async fn forward(&self, message: Value) {
        let id = message
            .get("id")
            .filter(|_| message.get("method").is_some())
            .cloned();
        let signed_out = format!(
            "{} is signed out. Sign in again in Harness: Settings > Plugins > Connectors.",
            self.title
        );
        let mut response = match self.token(false).await {
            Ok(token) => self.request(&message, token.as_deref()).await,
            Err(_) => Err(ConnectorError::Login(signed_out.clone())),
        };
        if self.oauth
            && matches!(&response, Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED)
        {
            response = match self.token(true).await {
                Ok(token) => self.request(&message, token.as_deref()).await,
                Err(_) => Err(ConnectorError::Login(signed_out.clone())),
            };
        }
        let response = match response {
            Ok(r) => r,
            Err(error) => {
                self.reply_error(id.as_ref(), &error.to_string()).await;
                return;
            }
        };
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().await = Some(session.to_owned());
        }
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.reply_error(id.as_ref(), &signed_out).await;
            return;
        }
        if !status.is_success() {
            self.reply_error(id.as_ref(), &format!("{} answered {status}", self.title))
                .await;
            return;
        }
        let stream = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("text/event-stream"));
        if stream {
            self.relay_stream(response).await;
        } else if let Ok(body) = response.text().await {
            if !body.trim().is_empty() {
                self.relay(body.trim()).await;
            }
        }
    }

    async fn relay_stream(&self, mut response: reqwest::Response) {
        let mut sse = Sse::default();
        while let Ok(Some(chunk)) = response.chunk().await {
            for event in sse.push(&chunk) {
                self.relay(&event).await;
            }
        }
        if let Some(event) = sse.finish() {
            self.relay(&event).await;
        }
    }

    /// Writes one JSON-RPC message (or batch) as a line; notes the negotiated protocol version.
    async fn relay(&self, text: &str) {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if let Some(version) = value
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
        {
            *self.protocol.lock().await = Some(version.to_owned());
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    self.write(&item.to_string()).await;
                }
            }
            other => self.write(&other.to_string()).await,
        }
    }

    /// Ends the remote session (best effort).
    async fn close(&self) {
        let Some(session) = self.session.lock().await.clone() else {
            return;
        };
        let mut request = self
            .http
            .delete(&self.url)
            .header("mcp-session-id", session);
        if let Ok(Some(token)) = self.token(false).await {
            request = request.bearer_auth(token);
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), request.send()).await;
    }
}
