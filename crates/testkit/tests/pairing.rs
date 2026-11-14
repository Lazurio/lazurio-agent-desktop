//! Pairing against a fake Launchpad (contract v0 section 1, result of v1).

mod common;

use common::Setup;
use lazurio_agent_desktop_core::keystore::SecretStore;
use lazurio_agent_desktop_core::Error;
use lazurio_agent_desktop_testkit::{browse, Approval, FakeLaunchpad};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

fn approval(channel_url: Option<String>) -> Approval {
    Approval {
        device_id: "test-computer".into(),
        channel_url,
        decline: false,
        environment_name: None,
        registry: None,
    }
}

#[test]
fn pairs_and_keeps_the_key_in_the_store() {
    let dir = tempfile::tempdir().unwrap();
    // The channel must be on the Launchpad's host (127.0.0.1); the port may differ.
    let lp = FakeLaunchpad::start(Approval {
        environment_name: Some("Pracovní Prostředí".into()),
        ..approval(Some("ws://127.0.0.1:9/.lazurio/computers/attach".into()))
    });
    let (manager, store) = common::open(dir.path(), Setup::default());
    let view = common::pair(&manager, &lp.origin);
    manager.set_enabled(&view.id, false).unwrap();

    let req = lp.requests.lock().unwrap()[0].clone();
    assert_eq!(req["name"], "Test computer");
    assert_eq!(req["os"], "macos");
    assert!(req["pubkey"].starts_with("ssh-ed25519 "));
    assert_eq!(view.device_id, "test-computer");
    assert_eq!(view.label, "Pracovní Prostředí");
    assert_eq!(view.host, "127.0.0.1");
    assert!(view.key_fingerprint.starts_with("SHA256:"));
    assert_eq!(
        store
            .get(&format!("env:{}", view.id))
            .unwrap()
            .map(|s| s.len()),
        Some(32)
    );

    // Nothing secret and nothing SSH in the data directory.
    let config = std::fs::read_to_string(dir.path().join("environments.json")).unwrap();
    assert!(config.contains("ws://127.0.0.1:9/.lazurio/computers/attach"));
    assert!(!config.contains("PRIVATE KEY"));
    assert!(!config.contains("knownHosts"));

    manager.remove(&view.id).unwrap();
    assert!(manager.environments().is_empty());
    assert_eq!(store.get(&format!("env:{}", view.id)).unwrap(), None);
}

#[test]
fn a_v0_result_gets_its_channel_from_the_launchpad() {
    let dir = tempfile::tempdir().unwrap();
    let lp = FakeLaunchpad::start(approval(None));
    let (manager, _) = common::open(dir.path(), Setup::default());
    let view = common::pair(&manager, &lp.origin);
    manager.set_enabled(&view.id, false).unwrap();
    let config = std::fs::read_to_string(dir.path().join("environments.json")).unwrap();
    let expected = format!(
        "{}/.lazurio/computers/attach",
        lp.origin.replace("http://", "ws://")
    );
    assert!(config.contains(&expected), "{config}");
    // Without a name from Launchpad the panel shows the host, never the port.
    assert_eq!(view.label, "127.0.0.1");
    manager.shutdown();
}

#[test]
fn ignores_a_callback_with_another_state() {
    let dir = tempfile::tempdir().unwrap();
    let lp = FakeLaunchpad::start(approval(None));
    let (manager, _) = common::open(dir.path(), Setup::default());
    let pending = manager.begin_pairing(&lp.origin).unwrap();
    let url = url::Url::parse(&pending.url).unwrap();
    let callback = url
        .query_pairs()
        .find(|(k, _)| k == "callback")
        .unwrap()
        .1
        .to_string();
    let real = pending.url.clone();
    let browser = std::thread::spawn(move || {
        let forged = format!("{callback}?state=forged&result=e30");
        (browse(&forged), browse(&real))
    });
    let view = manager
        .complete_pairing(pending, Duration::from_secs(20), &AtomicBool::new(false))
        .unwrap();
    let ((forged_status, _), (real_status, _)) = browser.join().unwrap();
    assert_eq!(forged_status, 400);
    assert_eq!(real_status, 200);
    assert_eq!(view.device_id, "test-computer");
    manager.shutdown();
}

#[test]
fn reports_a_cancelled_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let lp = FakeLaunchpad::start(Approval {
        decline: true,
        ..approval(None)
    });
    let (manager, _) = common::open(dir.path(), Setup::default());
    let pending = manager.begin_pairing(&lp.origin).unwrap();
    let url = pending.url.clone();
    let browser = std::thread::spawn(move || browse(&url));
    let err = manager
        .complete_pairing(pending, Duration::from_secs(20), &AtomicBool::new(false))
        .unwrap_err();
    assert!(matches!(err, Error::PairingDeclined), "{err}");
    assert_eq!(browser.join().unwrap().0, 200);
    assert!(manager.environments().is_empty());
}

#[test]
fn can_be_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = common::open(dir.path(), Setup::default());
    let pending = manager
        .begin_pairing("https://launchpad.example.com")
        .unwrap();
    let err = manager
        .complete_pairing(pending, Duration::from_secs(20), &AtomicBool::new(true))
        .unwrap_err();
    assert!(matches!(err, Error::PairingCancelled));
}
