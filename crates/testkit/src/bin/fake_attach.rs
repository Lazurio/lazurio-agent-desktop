//! Stands in for the Environment's `lazurio computer attach`.
//!
//! It runs either as the forced command of a real `sshd` (`--out FILE`) or in
//! place of the `ssh` program itself, when it receives the ssh arguments and
//! reads its settings from `-o LadOut=FILE`, `-o LadHoldMs=N` and `-o LadReject=1`.
//!
//! It initializes the computer's MCP server, pings it like the Environment's
//! broker does every 15 s, lists its tools, calls one tool
//! that succeeds and one that fails, appends a summary line to the out file and exits after the
//! hold time, which ends the session like a dropped connection would.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out_file = None;
    let mut hold_ms = 0u64;
    let mut reject = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let next = args.get(i + 1).cloned().unwrap_or_default();
        if a == "--out" {
            out_file = Some(next);
            i += 1;
        } else if a == "--hold-ms" {
            hold_ms = next.parse().unwrap_or(0);
            i += 1;
        } else if a == "-o" {
            if let Some(v) = next.strip_prefix("LadOut=") {
                out_file = Some(v.to_string());
            } else if let Some(v) = next.strip_prefix("LadHoldMs=") {
                hold_ms = v.parse().unwrap_or(0);
            } else if next == "LadReject=1" {
                reject = true;
            }
            i += 1;
        }
        i += 1;
    }
    if reject {
        eprintln!("Počítač test je už připojený jinou relací.");
        std::process::exit(2);
    }

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut out = std::io::stdout().lock();
    let mut send = |v: Value| {
        writeln!(out, "{v}").unwrap();
        out.flush().unwrap();
    };
    let mut wait_for = |id: &str| -> Value {
        for line in lines.by_ref() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v["id"] == id {
                    return v;
                }
            }
        }
        Value::Null
    };

    send(
        json!({"jsonrpc": "2.0", "id": "b0", "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "fake-attach", "version": "0"}}}),
    );
    let init = wait_for("b0");
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(json!({"jsonrpc": "2.0", "id": "p1", "method": "ping"}));
    let ping = wait_for("p1");
    send(json!({"jsonrpc": "2.0", "id": "l1", "method": "tools/list", "params": {}}));
    let list = wait_for("l1");
    let tool_names: Vec<String> = list["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let app_status = if tool_names.iter().any(|n| n == "lazurio_app_status") {
        send(
            json!({"jsonrpc": "2.0", "id": "s1", "method": "tools/call", "params": {
            "name": "lazurio_app_status", "arguments": {}}}),
        );
        wait_for("s1")["result"]["content"][0]["text"].clone()
    } else {
        Value::Null
    };
    send(
        json!({"jsonrpc": "2.0", "id": "c1", "method": "tools/call", "params": {
        "name": "desktop_fake_tool", "arguments": {"app": "Fake App", "text": "do-not-log-this"}}}),
    );
    let c1 = wait_for("c1");
    send(
        json!({"jsonrpc": "2.0", "id": "c2", "method": "tools/call", "params": {
        "name": "desktop_fail", "arguments": {}}}),
    );
    let c2 = wait_for("c2");

    let summary = json!({
        "server": init["result"]["serverInfo"]["name"],
        "ping": ping["result"].is_object(),
        "app_status": app_status,
        "app_meta": init["result"]["_meta"]["io.lazurio/agent-desktop"],
        "tools": list["result"]["tools"].as_array().map_or(0, Vec::len),
        "c1_ok": c1["result"]["isError"] == false,
        "c2_error": c2["result"]["isError"] == true,
    });
    if let Some(path) = out_file {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        // One write, so a reader never sees half a line.
        f.write_all(format!("{summary}\n").as_bytes()).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(hold_ms));
}
