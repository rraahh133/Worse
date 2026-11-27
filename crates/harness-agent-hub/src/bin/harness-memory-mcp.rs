//! `harness-memory-mcp [--project <dir>] [--session <id>]` — stdio MCP server exposing `memory_read` /
//! `memory_write` and the per-session `session_notes_read` / `session_notes_write` (`--session`, else
//! `HARNESS_SESSION_ID`).
//! `harness-memory-mcp --delegate` — the `harness-delegate` server (phase-9-parity.md §9.8), configured by the
//! `HARNESS_DELEGATE_*` env vars the hub passes.
//! Data directory: `HARNESS_HOME` (else `~/.harness`).
//!
//! Standalone development binary: the shipped app runs the same code as `harness-slint --memory-mcp` /
//! `harness-slint --delegate` (§9.9, `harness_agent_hub::helpers`).

use std::process::ExitCode;

use harness_agent_hub::helpers;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == helpers::FLAG_DELEGATE) {
        return helpers::run_delegate_mcp();
    }
    if args.iter().any(|a| a == helpers::FLAG_TOOL_GATEWAY) {
        return helpers::run_tool_gateway();
    }
    if args.first().map(String::as_str) == Some(helpers::FLAG_CONNECTOR_MCP) {
        return helpers::run_connector_mcp(args.into_iter().skip(1));
    }
    helpers::run_memory_mcp(args)
}
