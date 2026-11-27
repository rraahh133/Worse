//! `harness-tools`: one MCP server that fronts the user's overlay servers (`mcp.json`) for agents that break
//! or drop tools when they get hundreds of them (agy, the API agent). The agent sees two tools, `find_tools`
//! and `call_tool`; the real servers start on first use, inside this process tree.
//!
//! The gateway reads `mcp.json` itself (`HARNESS_HOME`, `HARNESS_GATEWAY_AGENT`), so no server env or key
//! passes through the agent's `session/new`.

mod upstream;

use std::collections::HashMap;
use std::io::{BufRead, Write};

use harness_agent_config::McpServer;
use serde_json::{json, Value};

use upstream::Upstream;

/// Most matches `find_tools` returns.
const MAX_MATCHES: usize = 25;
const DEFAULT_MATCHES: usize = 8;
/// Longest tool description returned.
const DESCRIPTION_CAP: usize = 400;

/// The gateway state: the servers it fronts and those already running.
pub struct Gateway {
    servers: Vec<McpServer>,
    live: HashMap<String, Upstream>,
    failed: HashMap<String, String>,
}

impl Gateway {
    /// Fronts `servers` (stdio entries; others are ignored).
    #[must_use]
    pub fn new(servers: Vec<McpServer>) -> Self {
        let servers = servers
            .into_iter()
            .filter(|s| s.command.as_deref().is_some_and(|c| !c.trim().is_empty()))
            .collect();
        Self {
            servers,
            live: HashMap::new(),
            failed: HashMap::new(),
        }
    }

    fn names(&self) -> Vec<&str> {
        self.servers.iter().map(|s| s.name.as_str()).collect()
    }

    /// Starts every fronted server in `only` (all when empty) that is not running, in parallel.
    fn ensure(&mut self, only: Option<&str>) {
        self.live.retain(|_, up| up.alive());
        let todo: Vec<&McpServer> = self
            .servers
            .iter()
            .filter(|s| only.map_or(true, |name| s.name == name))
            .filter(|s| !self.live.contains_key(&s.name))
            .collect();
        let started: Vec<(String, Result<Upstream, String>)> = std::thread::scope(|scope| {
            let jobs: Vec<_> = todo
                .iter()
                .map(|s| (s.name.clone(), scope.spawn(|| Upstream::start(s))))
                .collect();
            jobs.into_iter()
                .map(|(name, job)| {
                    let result = job.join().unwrap_or_else(|_| Err("start panicked".into()));
                    (name, result)
                })
                .collect()
        });
        for (name, result) in started {
            match result {
                Ok(up) => {
                    self.failed.remove(&name);
                    self.live.insert(name, up);
                }
                Err(error) => {
                    self.failed.insert(name, error);
                }
            }
        }
    }

    fn find(&mut self, args: &Value) -> Value {
        let server = args.get("server").and_then(Value::as_str);
        self.ensure(server);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(DEFAULT_MATCHES)
            .clamp(1, MAX_MATCHES);
        let words: Vec<String> = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect();
        let mut hits: Vec<(usize, &str, &Value)> = Vec::new();
        for (name, up) in &self.live {
            if server.is_some_and(|s| s != name) {
                continue;
            }
            for tool in &up.tools {
                let score = score(tool, name, &words);
                if score > 0 || words.is_empty() {
                    hits.push((score, name.as_str(), tool));
                }
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let total = hits.len();
        let tools: Vec<Value> = hits
            .into_iter()
            .take(limit)
            .map(|(_, server, tool)| {
                let description: String = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .chars()
                    .take(DESCRIPTION_CAP)
                    .collect();
                json!({
                    "server": server,
                    "tool": tool.get("name").cloned().unwrap_or(Value::Null),
                    "description": description,
                    "inputSchema": tool.get("inputSchema").cloned().unwrap_or_else(|| json!({}))
                })
            })
            .collect();
        let unavailable: Value = self
            .failed
            .iter()
            .filter(|(name, _)| server.map_or(true, |s| s == name.as_str()))
            .map(|(name, error)| (name.clone(), Value::String(error.clone())))
            .collect::<serde_json::Map<_, _>>()
            .into();
        json!({"matches": total, "tools": tools, "unavailable": unavailable})
    }

    fn call(&mut self, args: &Value) -> Result<Value, String> {
        let server = args
            .get("server")
            .and_then(Value::as_str)
            .ok_or("`server` is required")?;
        let tool = args
            .get("tool")
            .and_then(Value::as_str)
            .ok_or("`tool` is required")?;
        if !self.servers.iter().any(|s| s.name == server) {
            return Err(format!(
                "unknown server {server}; servers: {}",
                self.names().join(", ")
            ));
        }
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        self.ensure(Some(server));
        let up = self.live.get_mut(server).ok_or_else(|| {
            let why = self.failed.get(server).map_or("", String::as_str);
            format!("server {server} is not available: {why}")
        })?;
        up.call(tool, &arguments)
    }

    fn definitions(&self) -> Value {
        let servers = self.names().join(", ");
        json!([
            {
                "name": "find_tools",
                "description": format!("Search the tools of the user's other MCP servers ({servers}). Returns matching tools with their input schema; then run one with call_tool."),
                "inputSchema": {"type": "object", "properties": {
                    "query": {"type": "string", "description": "What you need, e.g. \"deploy application\""},
                    "server": {"type": "string", "description": "Only this server"},
                    "limit": {"type": "integer"}
                }, "required": []},
                "_meta": {"antigravity/alwaysLoad": true}
            },
            {
                "name": "call_tool",
                "description": "Run a tool found with find_tools: server, tool name and its arguments object.",
                "inputSchema": {"type": "object", "properties": {
                    "server": {"type": "string"},
                    "tool": {"type": "string"},
                    "arguments": {"type": "object"}
                }, "required": ["server", "tool"]},
                "_meta": {"antigravity/alwaysLoad": true}
            }
        ])
    }

    /// Answer to one JSON-RPC request; `None` for notifications.
    pub fn handle(&mut self, request: &Value) -> Option<Value> {
        let id = request.get("id")?.clone();
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": harness_agent_config::overlay::GATEWAY_SERVER, "version": env!("CARGO_PKG_VERSION")},
                "instructions": format!("Tools of the user's MCP servers {} are reached through this server: call find_tools with what you need, then call_tool.", self.names().join(", "))
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.definitions()})),
            "tools/call" => {
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                Ok(match params.get("name").and_then(Value::as_str) {
                    Some("find_tools") => text_result(&self.find(&args).to_string(), false),
                    Some("call_tool") => match self.call(&args) {
                        Ok(result) => result,
                        Err(error) => text_result(&error, true),
                    },
                    other => text_result(&format!("unknown tool {other:?}"), true),
                })
            }
            _ => Err(json!({"code": -32601, "message": format!("unknown method {method}")})),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        })
    }

    /// Serves MCP over newline-delimited JSON until `input` ends.
    ///
    /// # Errors
    /// Reading `input` or writing `output` failed.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            let Ok(request) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if let Some(reply) = self.handle(&request) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// Query words found in the tool name (3 each) or its server name / description (1 each).
fn score(tool: &Value, server: &str, words: &[String]) -> usize {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let rest = format!(
        "{} {}",
        server.to_lowercase(),
        tool.get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase()
    );
    words
        .iter()
        .map(|w| {
            if name.contains(w.as_str()) {
                3
            } else {
                usize::from(rest.contains(w.as_str()))
            }
        })
        .sum()
}

#[cfg(test)]
mod tests;
