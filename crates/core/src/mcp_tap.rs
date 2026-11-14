//! Watches the MCP lines passing through the channel without changing them.
//!
//! It only reads what the panel and the history need: that the session was
//! initialized, which tool runs, in which app (for the live status only) and
//! whether it succeeded.

use serde::de::IgnoredAny;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapEvent {
    Initialized,
    ToolStarted { tool: String, app: Option<String> },
    ToolFinished { tool: String, ok: bool, ms: u64 },
}

#[derive(Default)]
pub struct Tap {
    initialize_id: Option<String>,
    pending: HashMap<String, (String, Instant)>,
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Params>,
}

#[derive(Deserialize)]
struct Params {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Arguments>,
}

#[derive(Deserialize)]
struct Arguments {
    #[serde(default)]
    app: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<IgnoredAny>,
    #[serde(default)]
    result: Option<ResultFlags>,
}

#[derive(Deserialize)]
struct ResultFlags {
    #[serde(rename = "isError", default)]
    is_error: Option<bool>,
}

impl Tap {
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// A line the Environment sent to the computer.
    pub fn from_environment(&mut self, line: &[u8]) -> Vec<TapEvent> {
        let Ok(req) = serde_json::from_slice::<Request>(line) else {
            return Vec::new();
        };
        let (Some(id), Some(method)) = (req.id, req.method) else {
            return Vec::new();
        };
        match method.as_str() {
            "initialize" => {
                self.initialize_id = Some(id.to_string());
                Vec::new()
            }
            "tools/call" => {
                let params = req.params;
                let tool = params
                    .as_ref()
                    .and_then(|p| p.name.clone())
                    .unwrap_or_else(|| "?".into());
                let app = params
                    .and_then(|p| p.arguments)
                    .and_then(|a| a.app)
                    .and_then(|v| v.as_str().map(str::to_string));
                self.pending
                    .insert(id.to_string(), (tool.clone(), Instant::now()));
                vec![TapEvent::ToolStarted { tool, app }]
            }
            _ => Vec::new(),
        }
    }

    /// A line the engine sent back to the Environment.
    pub fn from_engine(&mut self, line: &[u8]) -> Vec<TapEvent> {
        if self.initialize_id.is_none() && self.pending.is_empty() {
            return Vec::new();
        }
        let Ok(resp) = serde_json::from_slice::<Response>(line) else {
            return Vec::new();
        };
        let Some(id) = resp.id.map(|v| v.to_string()) else {
            return Vec::new();
        };
        if self.initialize_id.as_deref() == Some(id.as_str()) {
            self.initialize_id = None;
            return if resp.error.is_none() {
                vec![TapEvent::Initialized]
            } else {
                Vec::new()
            };
        }
        let Some((tool, started)) = self.pending.remove(&id) else {
            return Vec::new();
        };
        let ok = resp.error.is_none() && !resp.result.and_then(|r| r.is_error).unwrap_or(false);
        vec![TapEvent::ToolFinished {
            tool,
            ok,
            ms: started.elapsed().as_millis() as u64,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_initialize_and_tool_calls() {
        let mut tap = Tap::default();
        assert!(tap
            .from_environment(br#"{"jsonrpc":"2.0","id":"b0","method":"initialize","params":{}}"#)
            .is_empty());
        assert_eq!(
            tap.from_engine(br#"{"jsonrpc":"2.0","id":"b0","result":{"serverInfo":{}}}"#),
            vec![TapEvent::Initialized]
        );
        assert_eq!(
            tap.from_environment(
                br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"desktop_type","arguments":{"app":"TextEdit","text":"secret"}}}"#
            ),
            vec![TapEvent::ToolStarted {
                tool: "desktop_type".into(),
                app: Some("TextEdit".into())
            }]
        );
        assert_eq!(tap.pending(), 1);
        let done = tap.from_engine(
            br#"{"jsonrpc":"2.0","id":7,"result":{"isError":true,"content":[{"type":"text","text":"x"}]}}"#,
        );
        assert!(
            matches!(&done[..], [TapEvent::ToolFinished { tool, ok: false, .. }] if tool == "desktop_type")
        );
        assert_eq!(tap.pending(), 0);
    }

    #[test]
    fn ignores_noise() {
        let mut tap = Tap::default();
        assert!(tap.from_environment(b"not json").is_empty());
        assert!(tap
            .from_environment(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_empty());
        assert!(tap
            .from_engine(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
            .is_empty());
    }
}
