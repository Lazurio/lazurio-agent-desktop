//! Self-update from the GitHub Releases of `Lazurio/lazurio-agent-desktop`.
//!
//! Three ways in, one install path:
//! - automatically: after start and every 4 hours, installed only while no
//!   agent is working on this computer,
//! - the Update button in the panel,
//! - the agent tools `lazurio_app_status` and `lazurio_app_update`, which the
//!   core serves next to the engine's tools on the same MCP session.
//!
//! The updater only accepts packages signed with the key in `tauri.conf.json`
//! (`plugins.updater.pubkey`) and only versions newer than the running one.
//! `latest.json` comes from the newest release that is not a pre-release.

use crate::AppState;
use lazurio_agent_desktop_core::app_tools::{text_result, AppTools};
use lazurio_agent_desktop_core::channel::State;
use lazurio_agent_desktop_core::platform;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

pub const CHANNEL: &str = "stable (GitHub Releases, Lazurio/lazurio-agent-desktop)";
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(4 * 60 * 60);
/// While an agent works, a found update waits and is retried this often.
const RETRY_WHEN_BUSY: Duration = Duration::from_secs(5 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current: String,
    pub available: Option<String>,
    pub notes: Option<String>,
    pub installing: bool,
    pub last_check_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Updates {
    info: Mutex<UpdateInfo>,
    pending: Mutex<Option<Update>>,
}

impl Updates {
    pub fn info(&self, app: &AppHandle) -> UpdateInfo {
        let mut info = self.info.lock().unwrap().clone();
        info.current = app.package_info().version.to_string();
        info
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn updates(app: &AppHandle) -> &Updates {
    &app.state::<AppState>().inner().updates
}

/// Asks GitHub Releases for a newer version and remembers the answer.
pub async fn check(app: &AppHandle) -> Result<Option<String>, String> {
    let result = async {
        let updater = app
            .updater_builder()
            .timeout(CHECK_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        updater.check().await.map_err(|e| e.to_string())
    }
    .await;
    let st = updates(app);
    let mut info = st.info.lock().unwrap();
    info.last_check_ms = Some(now_ms());
    let answer = match result {
        Ok(Some(update)) => {
            info.available = Some(update.version.clone());
            info.notes = update.body.clone();
            info.error = None;
            let version = update.version.clone();
            *st.pending.lock().unwrap() = Some(update);
            Ok(Some(version))
        }
        Ok(None) => {
            info.available = None;
            info.notes = None;
            info.error = None;
            *st.pending.lock().unwrap() = None;
            Ok(None)
        }
        Err(e) => {
            info.error = Some(e.clone());
            Err(e)
        }
    };
    drop(info);
    let _ = app.emit("changed", ());
    answer
}

/// Whether an agent is working on this computer now: a tool call running or
/// one within the last 20 seconds (the panel's "agent working" state).
pub fn agent_working(app: &AppHandle) -> bool {
    app.state::<AppState>()
        .manager
        .environments()
        .iter()
        .any(|e| e.status.state == State::Working)
}

/// Downloads, verifies, installs and restarts. Channels end before the
/// install and come back after the restart, because the switches are saved.
pub async fn install(app: AppHandle) -> Result<(), String> {
    let update = {
        let pending = updates(&app).pending.lock().unwrap().clone();
        match pending {
            Some(update) => update,
            None => match check(&app).await? {
                Some(_) => updates(&app)
                    .pending
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or("the update disappeared")?,
                None => return Err("no update available".into()),
            },
        }
    };
    {
        let st = updates(&app);
        let mut info = st.info.lock().unwrap();
        if info.installing {
            return Ok(());
        }
        info.installing = true;
        info.error = None;
    }
    let _ = app.emit("changed", ());
    let bytes = match update.download(|_, _| {}, || {}).await {
        Ok(bytes) => bytes,
        Err(e) => {
            let st = updates(&app);
            let mut info = st.info.lock().unwrap();
            info.installing = false;
            info.error = Some(e.to_string());
            drop(info);
            let _ = app.emit("changed", ());
            return Err(e.to_string());
        }
    };
    // From here on the old version stops: end the channels cleanly first.
    app.state::<AppState>().manager.shutdown();
    if let Err(e) = update.install(bytes) {
        // The channels are down; a restart of the current version brings them back.
        eprintln!("update install failed: {e}");
    }
    app.restart();
}

/// The automatic path: check after start and every 4 hours; install when no
/// agent is working, otherwise try again in a few minutes.
pub fn spawn_background(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_AFTER).await;
        loop {
            let next = match check(&app).await {
                Ok(Some(_)) => loop {
                    if !agent_working(&app) {
                        let _ = install(app.clone()).await;
                        break CHECK_EVERY;
                    }
                    tokio::time::sleep(RETRY_WHEN_BUSY).await;
                },
                _ => CHECK_EVERY,
            };
            tokio::time::sleep(next).await;
        }
    });
}

/// The app's own MCP tools, served by the core next to the engine's.
pub struct AgentTools {
    pub app: AppHandle,
}

impl AgentTools {
    fn status(&self, available: Option<String>, error: Option<String>) -> Value {
        json!({
            "app": "Lazurio Agent Desktop",
            "version": self.app.package_info().version.to_string(),
            "os": platform::os_name(),
            "osVersion": platform::os_version(),
            "channel": CHANNEL,
            "updateAvailable": available,
            "checkError": error,
        })
    }
}

impl AppTools for AgentTools {
    fn definitions(&self) -> Vec<Value> {
        vec![
            json!({
                "name": "lazurio_app_status",
                "description": "Version of Lazurio Agent Desktop on this computer, the system and its version, the update channel and whether a newer version is available (checks GitHub Releases now).",
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
            }),
            json!({
                "name": "lazurio_app_update",
                "description": "Updates Lazurio Agent Desktop on this computer to the newest stable version: downloads it, verifies its signature, installs it and restarts the app. The connection to this computer drops for about a minute while it restarts and then comes back by itself; wait, then call the computer's tools again. Does nothing when the app is already up to date.",
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
            }),
        ]
    }

    fn call(&self, name: &str, _arguments: &Value) -> Value {
        let checked = tauri::async_runtime::block_on(check(&self.app));
        match name {
            "lazurio_app_status" => {
                let (available, error) = match checked {
                    Ok(v) => (v, None),
                    Err(e) => (None, Some(e)),
                };
                text_result(
                    serde_json::to_string_pretty(&self.status(available, error))
                        .unwrap_or_default(),
                    false,
                )
            }
            "lazurio_app_update" => match checked {
                Ok(Some(version)) => {
                    let app = self.app.clone();
                    // Answer first, then go: the answer has to reach the agent
                    // before the connection drops.
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        let _ = install(app).await;
                    });
                    text_result(
                        format!(
                            "Updating Lazurio Agent Desktop from {} to {version}. The connection to this computer drops for about a minute while the app restarts, then it reconnects by itself.",
                            self.app.package_info().version
                        ),
                        false,
                    )
                }
                Ok(None) => text_result(
                    format!(
                        "Lazurio Agent Desktop {} is the newest stable version; nothing to update.",
                        self.app.package_info().version
                    ),
                    false,
                ),
                Err(e) => text_result(format!("Checking for an update failed: {e}"), true),
            },
            other => text_result(format!("Unknown tool {other}"), true),
        }
    }

    fn about(&self) -> Value {
        json!({
            "app": "Lazurio Agent Desktop",
            "appVersion": self.app.package_info().version.to_string(),
            "os": platform::os_name(),
            "osVersion": platform::os_version(),
            "updateChannel": "stable",
        })
    }
}
