//! Tools the app serves itself, next to the engine's, on the same MCP session.
//!
//! The Environment sees one MCP server. `tools/list` answers get the app's
//! tools appended, `tools/call` of an app tool is answered here and never
//! reaches the engine, and the `initialize` answer carries the app's version
//! and system under `_meta["io.lazurio/agent-desktop"]`. Every other line
//! passes through unchanged.

use crate::lock::{ComputerLock, Holder};
use serde::Deserialize;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Key of the app's details in the `initialize` result's `_meta`.
pub const META_KEY: &str = "io.lazurio/agent-desktop";

pub trait AppTools: Send + Sync {
    /// MCP tool definitions: `name`, `description`, `inputSchema`.
    fn definitions(&self) -> Vec<Value>;
    /// The `tools/call` result object for one of the app's tools. May block
    /// (it runs on its own thread) but must return.
    fn call(&self, name: &str, arguments: &Value) -> Value;
    /// The app's version and system, for the Environment to show.
    fn about(&self) -> Value;
}

/// Shared handle with a `Debug` that does not try to print the tools.
#[derive(Clone)]
pub struct AppToolsHandle(pub Arc<dyn AppTools>);

impl std::fmt::Debug for AppToolsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppTools")
    }
}

/// What to do with a line from the Environment.
pub(crate) enum Route {
    Engine,
    /// A tool call refused because another Environment holds the computer.
    Refuse {
        id: Value,
        holder: Holder,
    },
    /// An app tool call: answer it here.
    Local {
        id: Value,
        name: String,
        arguments: Value,
    },
}

#[derive(Deserialize)]
struct Incoming {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<IncomingParams>,
}

#[derive(Deserialize)]
struct IncomingParams {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
    #[serde(default)]
    cursor: Option<Value>,
}

#[derive(Deserialize)]
struct IdOnly {
    #[serde(default)]
    id: Option<Value>,
}

/// Tools that answer about the computer itself and never take it over.
const LOCK_FREE_TOOLS: &[&str] = &["lazurio_app_status"];

/// The Environment a router serves and the lock it shares with the others.
pub(crate) struct LockSeat {
    pub(crate) lock: Arc<ComputerLock>,
    pub(crate) env_id: String,
    pub(crate) label: String,
}

pub(crate) struct Router {
    tools: Option<Arc<dyn AppTools>>,
    names: HashSet<String>,
    list_ids: Mutex<HashSet<String>>,
    init_ids: Mutex<HashSet<String>>,
    seat: Option<LockSeat>,
    /// The "commands on this computer" switch of this Environment.
    shell: Option<Arc<AtomicBool>>,
}

impl Router {
    /// Also serves the `lazurio_shell_*` tools, listed while `enabled` is on.
    pub(crate) fn with_shell(mut self, enabled: Arc<AtomicBool>) -> Self {
        self.shell = Some(enabled);
        self
    }

    fn shell_listed(&self) -> bool {
        self.shell
            .as_ref()
            .is_some_and(|s| s.load(Ordering::SeqCst))
    }
    pub(crate) fn with_lock(tools: Option<Arc<dyn AppTools>>, seat: LockSeat) -> Self {
        Self {
            seat: Some(seat),
            ..Self::new(tools)
        }
    }

    pub(crate) fn new(tools: Option<Arc<dyn AppTools>>) -> Self {
        let names = tools
            .as_ref()
            .map(|t| {
                t.definitions()
                    .iter()
                    .filter_map(|d| d["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            tools,
            names,
            list_ids: Mutex::new(HashSet::new()),
            init_ids: Mutex::new(HashSet::new()),
            seat: None,
            shell: None,
        }
    }

    /// The computer-wide lock for a tool call; `Err` with the holder when
    /// another Environment has the computer.
    fn take_lock(&self, tool: Option<&str>) -> Result<(), Holder> {
        let Some(seat) = &self.seat else {
            return Ok(());
        };
        if tool.is_some_and(|t| LOCK_FREE_TOOLS.contains(&t)) {
            return Ok(());
        }
        seat.lock.try_begin(&seat.env_id, &seat.label)
    }

    /// Whether a tool call took the lock (and so must give it back when done).
    pub(crate) fn holds_for(&self, tool: &str) -> bool {
        self.seat.is_some() && !LOCK_FREE_TOOLS.contains(&tool)
    }

    pub(crate) fn tools(&self) -> Option<&Arc<dyn AppTools>> {
        self.tools.as_ref()
    }

    pub(crate) fn route_environment_line(&self, line: &[u8]) -> Route {
        if self.tools.is_none() && self.seat.is_none() && self.shell.is_none() {
            return Route::Engine;
        }
        let Ok(msg) = serde_json::from_slice::<Incoming>(line) else {
            return Route::Engine;
        };
        let (Some(id), Some(method)) = (msg.id, msg.method) else {
            return Route::Engine;
        };
        match method.as_str() {
            "initialize" => {
                self.init_ids.lock().unwrap().insert(id.to_string());
            }
            "tools/list" => {
                // Only the first page; later pages are the engine's alone.
                if msg.params.as_ref().is_none_or(|p| p.cursor.is_none()) {
                    self.list_ids.lock().unwrap().insert(id.to_string());
                }
            }
            "tools/call" => {
                let params = msg.params;
                let name = params.as_ref().and_then(|p| p.name.clone());
                if let Err(holder) = self.take_lock(name.as_deref()) {
                    return Route::Refuse { id, holder };
                }
                let local = |n: &String| {
                    self.names.contains(n)
                        || (self.shell.is_some() && crate::shell::is_shell_tool(n))
                };
                if let Some(name) = name.filter(local) {
                    let arguments = params
                        .and_then(|p| p.arguments)
                        .unwrap_or_else(|| json!({}));
                    return Route::Local {
                        id,
                        name,
                        arguments,
                    };
                }
            }
            _ => {}
        }
        Route::Engine
    }

    /// A line from the engine, with the app's additions where they belong.
    pub(crate) fn rewrite_engine_line<'a>(&self, line: &'a [u8]) -> Cow<'a, [u8]> {
        if self.tools.is_none() && self.shell.is_none() {
            return Cow::Borrowed(line);
        }
        let (listing, initializing) = (
            !self.list_ids.lock().unwrap().is_empty(),
            !self.init_ids.lock().unwrap().is_empty(),
        );
        if !listing && !initializing {
            return Cow::Borrowed(line);
        }
        let Some(id) = serde_json::from_slice::<IdOnly>(line)
            .ok()
            .and_then(|m| m.id)
            .map(|id| id.to_string())
        else {
            return Cow::Borrowed(line);
        };
        let is_list = self.list_ids.lock().unwrap().remove(&id);
        let is_init = !is_list && self.init_ids.lock().unwrap().remove(&id);
        if !is_list && !is_init {
            return Cow::Borrowed(line);
        }
        let Ok(mut msg) = serde_json::from_slice::<Value>(line) else {
            return Cow::Borrowed(line);
        };
        let Some(result) = msg.get_mut("result").and_then(Value::as_object_mut) else {
            return Cow::Borrowed(line);
        };
        if is_list {
            if let Some(list) = result.get_mut("tools").and_then(Value::as_array_mut) {
                if let Some(tools) = &self.tools {
                    list.extend(tools.definitions());
                }
                if self.shell_listed() {
                    list.extend(crate::shell::definitions());
                }
            }
        } else if let Some(tools) = &self.tools {
            let meta = result.entry("_meta").or_insert_with(|| json!({}));
            if let Some(meta) = meta.as_object_mut() {
                meta.insert(META_KEY.to_string(), tools.about());
            }
        }
        let mut out = serde_json::to_vec(&msg).unwrap_or_else(|_| line.to_vec());
        out.push(b'\n');
        Cow::Owned(out)
    }
}

/// A `tools/call` result with one text block.
pub fn text_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": is_error })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    impl AppTools for Fake {
        fn definitions(&self) -> Vec<Value> {
            vec![json!({"name": "lazurio_app_status", "inputSchema": {"type": "object"}})]
        }
        fn call(&self, name: &str, _: &Value) -> Value {
            text_result(format!("called {name}"), false)
        }
        fn about(&self) -> Value {
            json!({"appVersion": "9.9.9"})
        }
    }

    fn router() -> Router {
        Router::new(Some(Arc::new(Fake)))
    }

    #[test]
    fn appends_tools_to_the_first_list_page_only() {
        let r = router();
        assert!(matches!(
            r.route_environment_line(
                br#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#
            ),
            Route::Engine
        ));
        let out = r.rewrite_engine_line(
            br#"{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"desktop_click"}]}}"#,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        let names: Vec<_> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["desktop_click", "lazurio_app_status"]);
        assert!(out.ends_with(b"\n"));
        r.route_environment_line(
            br#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{"cursor":"x"}}"#,
        );
        let page = br#"{"jsonrpc":"2.0","id":4,"result":{"tools":[]}}"#;
        assert!(matches!(r.rewrite_engine_line(page), Cow::Borrowed(_)));
    }

    #[test]
    fn answers_its_own_calls_and_forwards_the_rest() {
        let r = router();
        match r.route_environment_line(
            br#"{"jsonrpc":"2.0","id":"a","method":"tools/call","params":{"name":"lazurio_app_status","arguments":{"x":1}}}"#,
        ) {
            Route::Local { id, name, arguments } => {
                assert_eq!(id, json!("a"));
                assert_eq!(name, "lazurio_app_status");
                assert_eq!(arguments, json!({"x":1}));
            }
            _ => panic!("app tool went to the engine"),
        }
        assert!(matches!(
            r.route_environment_line(
                br#"{"jsonrpc":"2.0","id":"b","method":"tools/call","params":{"name":"desktop_click"}}"#
            ),
            Route::Engine
        ));
    }

    #[test]
    fn adds_the_app_version_to_initialize() {
        let r = router();
        r.route_environment_line(
            br#"{"jsonrpc":"2.0","id":"b0","method":"initialize","params":{}}"#,
        );
        let out = r.rewrite_engine_line(
            br#"{"jsonrpc":"2.0","id":"b0","result":{"serverInfo":{"name":"agent-desktop"}}}"#,
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["result"]["serverInfo"]["name"], "agent-desktop");
        assert_eq!(v["result"]["_meta"][META_KEY]["appVersion"], "9.9.9");
    }

    #[test]
    fn without_app_tools_nothing_changes() {
        let r = Router::new(None);
        r.route_environment_line(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        let line = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        assert!(matches!(r.rewrite_engine_line(line), Cow::Borrowed(_)));
    }

    #[test]
    fn refuses_calls_while_another_environment_holds_the_computer() {
        let lock = Arc::new(ComputerLock::default());
        let seat = |id: &str, label: &str| LockSeat {
            lock: lock.clone(),
            env_id: id.into(),
            label: label.into(),
        };
        let a = Router::with_lock(Some(Arc::new(Fake)), seat("a", "Spectoda"));
        let b = Router::with_lock(Some(Arc::new(Fake)), seat("b", "Lumbio"));
        let call =
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"desktop_click"}}"#;
        assert!(matches!(a.route_environment_line(call), Route::Engine));
        match b.route_environment_line(call) {
            Route::Refuse { holder, .. } => assert_eq!(holder.label, "Spectoda"),
            _ => panic!("b must be refused while a works"),
        }
        // The status tool still answers for b, so its agent can see who works.
        assert!(matches!(
            b.route_environment_line(
                br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lazurio_app_status"}}"#
            ),
            Route::Local { .. }
        ));
        assert!(!b.holds_for("lazurio_app_status"));
        assert!(b.holds_for("desktop_click"));
    }
}
