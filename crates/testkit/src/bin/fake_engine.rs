//! Stands in for `agent-desktop mcp`: a tiny MCP server over stdio.
//!
//! `desktop_fail` returns a tool error; every other tool succeeds.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let reply = match method {
            "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-engine", "version": "0"}
            }}),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                {"name": "desktop_fake_tool", "inputSchema": {"type": "object"}},
                {"name": "desktop_fail", "inputSchema": {"type": "object"}}
            ]}}),
            "tools/call" => {
                let fail = msg["params"]["name"] == "desktop_fail";
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "isError": fail,
                    "content": [{"type": "text", "text": if fail { "failed" } else { "ok" }}]
                }})
            }
            _ => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown method"}})
            }
        };
        if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}
