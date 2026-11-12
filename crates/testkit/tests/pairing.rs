//! Pairing against a fake Launchpad (contract v0, section 1).

mod common;

use lazurio_agent_desktop_core::keystore::SecretStore;
use lazurio_agent_desktop_core::Error;
use lazurio_agent_desktop_testkit::{browse, Approval, FakeLaunchpad};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

fn approval() -> Approval {
    Approval {
        device_id: "test-computer".into(),
        host: "env.tailnet.example.com".into(),
        user: "env".into(),
        port: 22,
        known_hosts:
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl"
                .into(),
        decline: false,
        environment_name: None,
        on_approve: None,
    }
}

#[test]
fn pairs_and_keeps_the_key_in_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let lp = FakeLaunchpad::start(Approval {
        environment_name: Some("Pracovní Prostředí".into()),
        ..approval()
    });
    // The app never connects in this test: the switch is turned off right after pairing.
    let (manager, store) = common::open(dir.path(), dir.path().join("no-ssh"), vec![]);
    let view = common::pair(&manager, &lp.origin);
    manager.set_enabled(&view.id, false).unwrap();

    let req = lp.requests.lock().unwrap()[0].clone();
    assert_eq!(req["name"], "Test computer");
    assert_eq!(req["os"], "macos");
    assert!(req["pubkey"].starts_with("ssh-ed25519 "));
    assert_eq!(view.device_id, "test-computer");
    assert_eq!(view.label, "Pracovní Prostředí");
    assert_eq!(view.host, "env.tailnet.example.com");
    assert!(view.key_fingerprint.starts_with("SHA256:"));
    assert_eq!(
        store
            .get(&format!("env:{}", view.id))
            .unwrap()
            .map(|s| s.len()),
        Some(32)
    );

    // Nothing secret lands in the data directory.
    let config = std::fs::read_to_string(dir.path().join("environments.json")).unwrap();
    assert!(config.contains("env.tailnet.example.com ssh-ed25519 AAAA"));
    assert!(!config.contains("PRIVATE KEY"));

    manager.remove(&view.id).unwrap();
    assert!(manager.environments().is_empty());
    assert_eq!(store.get(&format!("env:{}", view.id)).unwrap(), None);
}

#[test]
fn ignores_a_callback_with_another_state() {
    let dir = tempfile::tempdir().unwrap();
    let lp = FakeLaunchpad::start(approval());
    let (manager, _) = common::open(dir.path(), dir.path().join("no-ssh"), vec![]);
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
        let first = browse(&forged);
        let second = browse(&real);
        (first, second)
    });
    let view = manager
        .complete_pairing(pending, Duration::from_secs(20), &AtomicBool::new(false))
        .unwrap();
    let ((forged_status, _), (real_status, _)) = browser.join().unwrap();
    // Without a name from Launchpad the panel shows the host, never the port.
    assert_eq!(view.label, "127.0.0.1");
    assert_eq!(forged_status, 400);
    assert_eq!(real_status, 200);
    assert_eq!(view.device_id, "test-computer");
    manager.shutdown();
}

#[test]
fn reports_a_declined_pairing() {
    let dir = tempfile::tempdir().unwrap();
    let lp = FakeLaunchpad::start(Approval {
        decline: true,
        ..approval()
    });
    let (manager, _) = common::open(dir.path(), dir.path().join("no-ssh"), vec![]);
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
    let (manager, _) = common::open(dir.path(), dir.path().join("no-ssh"), vec![]);
    let pending = manager
        .begin_pairing("https://launchpad.example.com")
        .unwrap();
    let err = manager
        .complete_pairing(pending, Duration::from_secs(20), &AtomicBool::new(true))
        .unwrap_err();
    assert!(matches!(err, Error::PairingCancelled));
}
