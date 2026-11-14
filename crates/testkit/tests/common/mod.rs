#![allow(dead_code)]

use lazurio_agent_desktop_core::app_tools::{text_result, AppTools, AppToolsHandle};
use lazurio_agent_desktop_core::channel::{EngineSpec, State, Timing};
use lazurio_agent_desktop_core::keystore::MemoryStore;
use lazurio_agent_desktop_core::manager::{
    ChangeListener, EnvironmentView, Manager, ManagerConfig,
};
use lazurio_agent_desktop_testkit::{browse, wait_until, Approval, FakeEnvironment, FakeLaunchpad};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

pub fn fake_engine() -> EngineSpec {
    EngineSpec {
        program: PathBuf::from(env!("CARGO_BIN_EXE_fake-engine")),
        args: vec!["mcp".into()],
        env: vec![],
    }
}

/// The real Agent Desktop engine when `LAD_ENGINE` points at its binary.
pub fn real_engine() -> Option<EngineSpec> {
    let program = PathBuf::from(std::env::var_os("LAD_ENGINE")?);
    Some(EngineSpec {
        program,
        args: vec!["mcp".into()],
        env: vec![],
    })
}

/// Short keep-alive so tests see dead connections quickly.
pub fn quick_timing() -> Timing {
    Timing {
        connect_timeout: Duration::from_secs(5),
        ping_every: Duration::from_millis(200),
        dead_after: Duration::from_secs(1),
    }
}

/// App tools like the app's, answering with fixed texts.
pub struct FakeAppTools;

impl AppTools for FakeAppTools {
    fn definitions(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "name": "lazurio_app_status",
            "description": "fake",
            "inputSchema": {"type": "object"}
        })]
    }
    fn call(&self, _name: &str, _arguments: &serde_json::Value) -> serde_json::Value {
        text_result("fake-app-status 9.9.9", false)
    }
    fn about(&self) -> serde_json::Value {
        serde_json::json!({"appVersion": "9.9.9", "os": "macos", "osVersion": "27.0"})
    }
}

pub fn app_tools() -> Option<AppToolsHandle> {
    Some(AppToolsHandle(Arc::new(FakeAppTools)))
}

pub struct Setup {
    pub engine: EngineSpec,
    pub app_tools: Option<AppToolsHandle>,
    pub timing: Timing,
    pub hold: Duration,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            engine: fake_engine(),
            app_tools: None,
            timing: quick_timing(),
            hold: Duration::from_secs(20),
        }
    }
}

pub fn open(data_dir: &Path, setup: Setup) -> (Arc<Manager>, Arc<MemoryStore>) {
    open_with(data_dir, setup, Arc::new(|| {}))
}

/// Like [`open`], with the app's change listener.
pub fn open_with(
    data_dir: &Path,
    setup: Setup,
    on_change: ChangeListener,
) -> (Arc<Manager>, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::default());
    let manager = Manager::open(
        ManagerConfig {
            data_dir: data_dir.to_path_buf(),
            engine: setup.engine,
            computer_name: "Test computer".into(),
            os: "macos".into(),
            app_tools: setup.app_tools,
            timing: setup.timing,
            hold_after_last_call: setup.hold,
        },
        store.clone(),
        on_change,
    )
    .unwrap();
    (manager, store)
}

/// A Launchpad that hands out `env`'s channel and registers keys with it.
pub fn launchpad_for(env: &FakeEnvironment, device_id: &str, name: Option<&str>) -> FakeLaunchpad {
    FakeLaunchpad::start(Approval {
        device_id: device_id.into(),
        channel_url: Some(env.channel_url.clone()),
        decline: false,
        environment_name: name.map(str::to_string),
        registry: Some(env.registry.clone()),
    })
}

/// Pairs like a person would: the app opens the URL, the browser follows Launchpad's redirect.
pub fn pair(manager: &Manager, origin: &str) -> EnvironmentView {
    let pending = manager.begin_pairing(origin).unwrap();
    let url = pending.url.clone();
    let browser = std::thread::spawn(move || browse(&url));
    let view = manager
        .complete_pairing(pending, Duration::from_secs(20), &AtomicBool::new(false))
        .unwrap();
    let (status, body) = browser.join().unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Počítač je připojený"));
    view
}

pub fn state_of(manager: &Manager, id: &str) -> State {
    manager
        .environments()
        .into_iter()
        .find(|e| e.id == id)
        .map(|e| e.status.state)
        .unwrap_or(State::Off)
}

/// Complete lines only: a line still being written does not count.
pub fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.matches('\n').count())
        .unwrap_or(0)
}

pub fn first_line(path: &Path) -> serde_json::Value {
    serde_json::from_str(
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap()
}

pub fn wait(timeout_s: u64, check: impl FnMut() -> bool) -> bool {
    wait_until(Duration::from_secs(timeout_s), check)
}
