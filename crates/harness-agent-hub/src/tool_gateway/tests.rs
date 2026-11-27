#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use harness_agent_config::{McpServer, McpTransport};
use serde_json::{json, Value};

use super::Gateway;

const FAKE: &str = "HARNESS_TEST_FAKE_MCP";

/// Not a real test: when `FAKE` is set, this test binary acts as an MCP server with 300 tools (used below).
#[test]
fn fake_mcp_server() {
    if std::env::var_os(FAKE).is_none() {
        return;
    }
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    // libtest already printed "test … " without a newline on this line.
    writeln!(out).unwrap();
    for line in stdin.lock().lines() {
        let Ok(request) = serde_json::from_str::<Value>(&line.unwrap()) else {
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let result = match request["method"].as_str().unwrap_or_default() {
            "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}}),
            "tools/list" => {
                let page = request["params"]["cursor"].as_u64().unwrap_or(0);
                let tools: Vec<Value> = (page * 150..page * 150 + 150)
                    .map(|i| json!({"name": format!("tool_{i}"), "description": format!("does thing {i}"),
                        "inputSchema": {"type": "object"}}))
                    .collect();
                let next = if page == 0 { json!(1) } else { Value::Null };
                json!({"tools": tools, "nextCursor": next})
            }
            "tools/call" => json!({"content": [{"type": "text",
                "text": format!("ran {} with {}", request["params"]["name"], request["params"]["arguments"])}]}),
            _ => json!({}),
        };
        writeln!(
            out,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        )
        .unwrap();
        out.flush().unwrap();
    }
    std::process::exit(0);
}

fn fake_server(name: &str) -> McpServer {
    let exe = std::env::current_exe().unwrap();
    McpServer {
        name: name.into(),
        transport: McpTransport::Stdio,
        command: Some(exe.to_string_lossy().into_owned()),
        args: vec![
            "--exact".into(),
            "tool_gateway::tests::fake_mcp_server".into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ],
        env: BTreeMap::from([(FAKE.to_owned(), "1".to_owned())]),
        url: None,
        headers: BTreeMap::new(),
        enabled: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        agents: Vec::new(),
        extra: serde_json::Map::new(),
    }
}

fn call(gateway: &mut Gateway, tool: &str, arguments: &Value) -> Value {
    let reply = gateway
        .handle(&json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}}))
        .unwrap();
    reply["result"].clone()
}

#[test]
fn agent_sees_two_tools_and_reaches_all_three_hundred() {
    let mut gateway = Gateway::new(vec![fake_server("big")]);
    let list = gateway
        .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .unwrap();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, ["find_tools", "call_tool"]);
    // No server starts until a tool is needed.
    assert!(gateway.live.is_empty());

    let found = call(&mut gateway, "find_tools", &json!({"query": "tool_299"}));
    let found: Value = serde_json::from_str(found["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(found["tools"][0]["tool"], "tool_299");
    assert_eq!(found["tools"][0]["server"], "big");
    assert_eq!(gateway.live["big"].tools.len(), 300);

    let ran = call(
        &mut gateway,
        "call_tool",
        &json!({"server": "big", "tool": "tool_299", "arguments": {"x": 1}}),
    );
    let text = ran["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("tool_299") && text.contains("\"x\":1"),
        "{text}"
    );
}

#[test]
fn bad_calls_are_tool_errors_not_crashes() {
    let mut broken = fake_server("broken");
    broken.command = Some("harness-no-such-command-xyz".into());
    let mut gateway = Gateway::new(vec![broken]);
    let unknown = call(
        &mut gateway,
        "call_tool",
        &json!({"server": "nope", "tool": "t"}),
    );
    assert_eq!(unknown["isError"], true);
    let down = call(
        &mut gateway,
        "call_tool",
        &json!({"server": "broken", "tool": "t"}),
    );
    assert_eq!(down["isError"], true);
    let found = call(&mut gateway, "find_tools", &json!({"query": "x"}));
    let found: Value = serde_json::from_str(found["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(found["unavailable"]["broken"].is_string());
}

#[test]
fn name_matches_rank_above_description_matches() {
    let words = vec!["deploy".to_owned()];
    let named = json!({"name": "application-deploy", "description": ""});
    let described = json!({"name": "other", "description": "deploy things"});
    assert!(super::score(&named, "s", &words) > super::score(&described, "s", &words));
}
