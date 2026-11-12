#![allow(dead_code)]

use lazurio_agent_desktop_core::channel::{EngineSpec, State};
use lazurio_agent_desktop_core::keystore::MemoryStore;
use lazurio_agent_desktop_core::manager::{EnvironmentView, Manager, ManagerConfig};
use lazurio_agent_desktop_testkit::{browse, wait_until};
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

pub fn fake_attach() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-attach"))
}

pub fn open(
    data_dir: &Path,
    ssh_program: PathBuf,
    extra: Vec<String>,
) -> (Arc<Manager>, Arc<MemoryStore>) {
    open_with_engine(data_dir, ssh_program, extra, fake_engine())
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

pub fn open_with_engine(
    data_dir: &Path,
    ssh_program: PathBuf,
    extra: Vec<String>,
    engine: EngineSpec,
) -> (Arc<Manager>, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::default());
    let manager = Manager::open(
        ManagerConfig {
            data_dir: data_dir.to_path_buf(),
            ssh_program,
            engine,
            computer_name: "Test computer".into(),
            os: "macos".into(),
            extra_ssh_options: extra,
        },
        store.clone(),
        Arc::new(|| {}),
    )
    .unwrap();
    (manager, store)
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

pub fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

pub fn no_key_files(data_dir: &Path) -> bool {
    std::fs::read_dir(data_dir.join("run"))
        .map(|d| {
            d.flatten()
                .all(|e| e.path().extension().is_none_or(|x| x != "key"))
        })
        .unwrap_or(true)
}

pub fn wait(timeout_s: u64, check: impl FnMut() -> bool) -> bool {
    wait_until(Duration::from_secs(timeout_s), check)
}
