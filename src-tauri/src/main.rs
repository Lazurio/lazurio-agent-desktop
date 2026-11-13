//! Lazurio Agent Desktop: tray icon and panel around the core.
//!
//! The interface is temporary; everything that has to survive the move into
//! Lazurio App lives in `lazurio-agent-desktop-core`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod permissions;
mod updates;

use lazurio_agent_desktop_core::app_tools::AppToolsHandle;
use lazurio_agent_desktop_core::channel::{EngineSpec, State};
use lazurio_agent_desktop_core::history::HistoryEntry;
use lazurio_agent_desktop_core::keystore;
use lazurio_agent_desktop_core::manager::PendingPairing;
use lazurio_agent_desktop_core::{platform, EnvironmentView, Manager, ManagerConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager as _, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_opener::OpenerExt;

const STARTED_AT_LOGIN: &str = "--at-login";
const PAIRING_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const TRAY_ID: &str = "main";

// macOS draws template icons in the menu bar's own colour; Windows needs a visible tile.
#[cfg(target_os = "macos")]
const TRAY_OFF: &[u8] = include_bytes!("../icons/tray-off.png");
#[cfg(target_os = "macos")]
const TRAY_ON: &[u8] = include_bytes!("../icons/tray-on.png");
#[cfg(target_os = "macos")]
const TRAY_BUSY: &[u8] = include_bytes!("../icons/tray-busy.png");
#[cfg(not(target_os = "macos"))]
const TRAY_OFF: &[u8] = include_bytes!("../icons/tray-win-off.png");
#[cfg(not(target_os = "macos"))]
const TRAY_ON: &[u8] = include_bytes!("../icons/tray-win-on.png");
#[cfg(not(target_os = "macos"))]
const TRAY_BUSY: &[u8] = include_bytes!("../icons/tray-win-busy.png");
const TRAY_TEMPLATE: bool = cfg!(target_os = "macos");

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    #[serde(default)]
    onboarded: bool,
    #[serde(default)]
    autostart_set: bool,
    /// The owner's update channel; none means the build's default.
    #[serde(default)]
    update_channel: Option<updates::Channel>,
}

struct AppState {
    manager: Arc<Manager>,
    pending: Mutex<Option<PendingPairing>>,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    updates: updates::Updates,
}

impl AppState {
    fn save_settings(&self) {
        if let Ok(bytes) = serde_json::to_vec_pretty(&*self.settings.lock().unwrap()) {
            let _ = platform::write_atomic(&self.settings_path, &bytes);
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CmdError {
    code: String,
    message: String,
}

impl From<lazurio_agent_desktop_core::Error> for CmdError {
    fn from(e: lazurio_agent_desktop_core::Error) -> Self {
        Self {
            code: e.code().into(),
            message: e.to_string(),
        }
    }
}

fn other(message: impl ToString) -> CmdError {
    CmdError {
        code: "other".into(),
        message: message.to_string(),
    }
}

type CmdResult<T> = Result<T, CmdError>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct View {
    environments: Vec<EnvironmentView>,
    os: &'static str,
    computer_name: String,
    version: &'static str,
    autostart: bool,
    onboarded: bool,
    pairing: bool,
    permissions: permissions::Status,
    update: updates::UpdateInfo,
}

#[tauri::command]
fn state(app: AppHandle) -> View {
    let st = app.state::<AppState>();
    let onboarded = st.settings.lock().unwrap().onboarded;
    let pairing = st.pending.lock().unwrap().is_some() || st.cancel.lock().unwrap().is_some();
    View {
        environments: st.manager.environments(),
        os: platform::os_name(),
        computer_name: platform::computer_name(),
        version: env!("CARGO_PKG_VERSION"),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        onboarded,
        pairing,
        permissions: permissions::status(),
        update: st.updates.info(&app),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingStarted {
    url: String,
    fingerprint: String,
}

/// Opens the approval page of the Environment's Launchpad in the browser.
#[tauri::command]
fn begin_pairing(app: AppHandle, launchpad: String) -> CmdResult<PairingStarted> {
    let st = app.state::<AppState>();
    if let Some(c) = st.cancel.lock().unwrap().take() {
        c.store(true, Ordering::SeqCst);
    }
    let pending = st.manager.begin_pairing(&launchpad)?;
    let started = PairingStarted {
        url: pending.url.clone(),
        fingerprint: pending.fingerprint.clone(),
    };
    app.opener()
        .open_url(&pending.url, None::<&str>)
        .map_err(other)?;
    *st.pending.lock().unwrap() = Some(pending);
    Ok(started)
}

/// Waits until the owner approves (or declines) in the browser.
#[tauri::command]
async fn wait_pairing(app: AppHandle) -> CmdResult<EnvironmentView> {
    let (manager, pending, cancel) = {
        let st = app.state::<AppState>();
        let pending = st
            .pending
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| other("no pairing in progress"))?;
        let cancel = Arc::new(AtomicBool::new(false));
        *st.cancel.lock().unwrap() = Some(cancel.clone());
        (st.manager.clone(), pending, cancel)
    };
    let done = {
        let cancel = cancel.clone();
        tauri::async_runtime::spawn_blocking(move || {
            manager.complete_pairing(pending, PAIRING_TIMEOUT, &cancel)
        })
        .await
        .map_err(other)?
    };
    let st = app.state::<AppState>();
    let mut slot = st.cancel.lock().unwrap();
    if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &cancel)) {
        *slot = None;
    }
    drop(slot);
    let _ = app.emit("changed", ());
    Ok(done?)
}

#[tauri::command]
fn cancel_pairing(app: AppHandle) {
    let st = app.state::<AppState>();
    st.pending.lock().unwrap().take();
    let cancel = st.cancel.lock().unwrap().take();
    if let Some(c) = cancel {
        c.store(true, Ordering::SeqCst);
    }
}

#[tauri::command]
fn set_enabled(app: AppHandle, id: String, enabled: bool) -> CmdResult<()> {
    Ok(app.state::<AppState>().manager.set_enabled(&id, enabled)?)
}

/// "Commands on this computer" for one Environment; the panel asks the
/// owner to confirm before turning it on.
#[tauri::command]
fn set_commands(app: AppHandle, id: String, enabled: bool) -> CmdResult<()> {
    Ok(app.state::<AppState>().manager.set_commands(&id, enabled)?)
}

#[tauri::command]
fn stop(app: AppHandle, id: String) -> CmdResult<()> {
    Ok(app.state::<AppState>().manager.stop(&id)?)
}

#[tauri::command]
fn stop_all(app: AppHandle) {
    app.state::<AppState>().manager.stop_all();
}

/// Off the main thread: removing ends the Environment's channel first.
#[tauri::command]
async fn remove_environment(app: AppHandle, id: String) -> CmdResult<()> {
    let manager = app.state::<AppState>().manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.remove(&id))
        .await
        .map_err(other)??;
    Ok(())
}

/// The page in Lazurio that lists this computer and can remove it there.
#[tauri::command]
fn open_launchpad(app: AppHandle, id: String) -> CmdResult<()> {
    let st = app.state::<AppState>();
    let env = st
        .manager
        .environments()
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| other("unknown environment"))?;
    let url = format!(
        "{}{}",
        env.launchpad,
        lazurio_agent_desktop_core::pairing::CONNECT_PATH
    );
    app.opener().open_url(url, None::<&str>).map_err(other)
}

#[tauri::command]
fn history(app: AppHandle, limit: Option<usize>) -> Vec<HistoryEntry> {
    app.state::<AppState>()
        .manager
        .history(limit.unwrap_or(100).min(500))
}

#[tauri::command]
fn request_permission(app: AppHandle, kind: String) {
    permissions::request(&kind);
    if let Some(url) = permissions::settings_url(&kind) {
        let _ = app.opener().open_url(url, None::<&str>);
    }
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> CmdResult<()> {
    let launcher = app.autolaunch();
    if enabled {
        launcher.enable().map_err(other)
    } else {
        launcher.disable().map_err(other)
    }
}

#[tauri::command]
fn finish_onboarding(app: AppHandle) {
    let st = app.state::<AppState>();
    st.settings.lock().unwrap().onboarded = true;
    st.save_settings();
}

/// The panel's Update button: install the announced version now.
#[tauri::command]
async fn install_update(app: AppHandle) -> CmdResult<()> {
    updates::install(app).await.map_err(other)
}

/// The panel's channel choice: stable or pilot.
#[tauri::command]
async fn set_update_channel(app: AppHandle, channel: String) -> CmdResult<()> {
    let channel = updates::Channel::parse(&channel).ok_or_else(|| other("unknown channel"))?;
    {
        let st = app.state::<AppState>();
        st.settings.lock().unwrap().update_channel = Some(channel);
        st.save_settings();
    }
    let _ = updates::check(&app).await;
    Ok(())
}

#[tauri::command]
async fn check_update(app: AppHandle) -> CmdResult<Option<String>> {
    updates::check(&app).await.map_err(other)
}

/// The panel's Quit. The channels end in `RunEvent::ExitRequested`.
#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

/// Ends every channel and its engine before the app goes. Runs on the main
/// thread and waits there at most `SHUTDOWN_GRACE`: a channel never needs the
/// main thread to end (the core tells the app about changes from a thread of
/// its own, and the app never waits for its main thread there), so quitting
/// cannot hang. Calling it again does nothing.
fn end_channels(app: &AppHandle) {
    if let Some(st) = app.try_state::<AppState>() {
        st.manager.shutdown();
    }
}

fn czech() -> bool {
    sys_locale::get_locale().is_some_and(|l| l.to_lowercase().starts_with("cs"))
}

fn show_panel(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn toggle_panel(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            show_panel(app);
        }
    }
}

/// Icon and tooltip show the busiest Environment: working, connected or off.
fn update_tray(app: &AppHandle) {
    let Some(st) = app.try_state::<AppState>() else {
        return;
    };
    let envs = st.manager.environments();
    let working = envs.iter().any(|e| e.status.state == State::Working);
    let connected = envs.iter().any(|e| e.status.state == State::Connected);
    let cs = czech();
    let (bytes, text) = if working {
        (
            TRAY_BUSY,
            if cs {
                "Pracuje agent"
            } else {
                "An agent is working"
            },
        )
    } else if connected {
        (TRAY_ON, if cs { "Připojeno" } else { "Connected" })
    } else {
        (
            TRAY_OFF,
            if cs {
                "Agenti nemají přístup"
            } else {
                "Agents have no access"
            },
        )
    };
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(img) = Image::from_bytes(bytes) {
            let _ = tray.set_icon(Some(img));
            let _ = tray.set_icon_as_template(TRAY_TEMPLATE);
        }
        let _ = tray.set_tooltip(Some(format!("Lazurio Agent Desktop: {text}")));
    }
}

fn engine_spec() -> EngineSpec {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_default();
    let program = dir.join(format!("agent-desktop{}", std::env::consts::EXE_SUFFIX));
    let mut env = Vec::new();
    if cfg!(target_os = "macos") {
        env.push((
            "AGENT_DESKTOP_MACOS_HELPER_PATH".to_string(),
            dir.join("agent-desktop-macos-helper")
                .to_string_lossy()
                .to_string(),
        ));
    }
    EngineSpec {
        program,
        args: vec!["mcp".into()],
        env,
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let cs = czech();
    let open = MenuItem::with_id(
        app,
        "open",
        if cs { "Otevřít panel" } else { "Open panel" },
        true,
        None::<&str>,
    )?;
    let stop = MenuItem::with_id(
        app,
        "stop_all",
        if cs { "Zastavit agenty" } else { "Stop agents" },
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        "quit",
        if cs {
            "Ukončit Lazurio Agent Desktop"
        } else {
            "Quit Lazurio Agent Desktop"
        },
        true,
        None::<&str>,
    )?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &stop, &sep, &quit])?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(TRAY_OFF)?)
        .icon_as_template(TRAY_TEMPLATE)
        .tooltip("Lazurio Agent Desktop")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_panel(app),
            "stop_all" => app.state::<AppState>().manager.stop_all(),
            // The channels end in `RunEvent::ExitRequested`.
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_panel(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_panel(app)
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![STARTED_AT_LOGIN]),
        ))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let data_dir = app.path().app_local_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let handle = app.handle().clone();
            let manager = Manager::open(
                ManagerConfig {
                    data_dir: data_dir.clone(),
                    engine: engine_spec(),
                    computer_name: platform::computer_name(),
                    os: platform::os_name().into(),
                    timing: lazurio_agent_desktop_core::channel::Timing::default(),
                    hold_after_last_call: lazurio_agent_desktop_core::lock::HOLD_AFTER_LAST_CALL,
                    app_tools: Some(AppToolsHandle(Arc::new(updates::AgentTools {
                        app: app.handle().clone(),
                    }))),
                },
                Arc::from(keystore::system_store(data_dir.join("keys"))),
                Arc::new(move || {
                    let _ = handle.emit("changed", ());
                    // Hand the tray to the main thread, never wait for it: the
                    // main thread may be quitting and waiting for the channels.
                    let tray_app = handle.clone();
                    let _ = handle.run_on_main_thread(move || update_tray(&tray_app));
                }),
            )?;
            let settings_path = data_dir.join("settings.json");
            let settings: Settings = std::fs::read(&settings_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let first_start = !settings.autostart_set;
            let has_envs = !manager.environments().is_empty();
            app.manage(AppState {
                manager,
                pending: Mutex::new(None),
                cancel: Mutex::new(None),
                settings: Mutex::new(settings),
                settings_path,
                updates: updates::Updates::default(),
            });
            let st = app.state::<AppState>();
            if first_start {
                // Start at login unless the owner turns it off in the panel.
                let _ = app.autolaunch().enable();
                st.settings.lock().unwrap().autostart_set = true;
                st.save_settings();
            }

            updates::spawn_background(app.handle().clone());
            build_tray(app.handle())?;
            update_tray(app.handle());

            let at_login = std::env::args().any(|a| a == STARTED_AT_LOGIN);
            if !at_login || !has_envs || !permissions::status().all_granted {
                show_panel(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the panel only hides it; the app lives in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            state,
            begin_pairing,
            wait_pairing,
            cancel_pairing,
            set_enabled,
            set_commands,
            stop,
            stop_all,
            remove_environment,
            open_launchpad,
            history,
            request_permission,
            set_autostart,
            finish_onboarding,
            quit,
            install_update,
            check_update,
            set_update_channel
        ])
        .build(tauri::generate_context!())
        .expect("failed to build the app")
        .run(|app, event| match event {
            // Closing the last window must not quit a tray app.
            tauri::RunEvent::ExitRequested {
                api, code: None, ..
            } => api.prevent_exit(),
            // Quit from the tray or the panel, the restart after an update.
            tauri::RunEvent::ExitRequested { .. } => end_channels(app),
            // macOS ends the app only this way when the system asks (Quit &
            // Reopen after granting a permission, log out, shut down), and
            // Windows when it shuts down: no ExitRequested comes first.
            tauri::RunEvent::Exit => end_channels(app),
            _ => {}
        });
}
