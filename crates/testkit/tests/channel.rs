//! The channel with `fake-attach` standing in for `ssh` plus the Environment.
//! Runs on every platform; `sshd.rs` repeats the important part over real OpenSSH.

mod common;

use common::{lines, no_key_files, state_of, wait};
use lazurio_agent_desktop_core::channel::{Reason, State};
use lazurio_agent_desktop_core::history::Event;
use lazurio_agent_desktop_testkit::{Approval, FakeLaunchpad};

fn launchpad() -> FakeLaunchpad {
    FakeLaunchpad::start(Approval {
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
    })
}

#[test]
fn serves_mcp_reconnects_and_obeys_the_switch() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("attach.jsonl");
    let lp = launchpad();
    let (manager, _) = common::open(
        dir.path(),
        common::fake_attach(),
        vec![format!("LadOut={}", out.display()), "LadHoldMs=200".into()],
    );
    let view = common::pair(&manager, &lp.origin);
    assert!(view.enabled);

    // The session ends after each attach; with the switch on the app connects again.
    assert!(wait(30, || lines(&out) >= 2), "no reconnect");
    let first: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first["server"], "fake-engine");
    assert_eq!(first["ping"], true);
    assert_eq!(first["tools"], 2);
    assert_eq!(first["c1_ok"], true);
    assert_eq!(first["c2_error"], true);

    manager.set_enabled(&view.id, false).unwrap();
    assert!(wait(10, || state_of(&manager, &view.id) == State::Off));
    let sessions = lines(&out);
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(lines(&out), sessions, "connected while switched off");
    assert!(no_key_files(dir.path()), "a key file was left behind");

    let history = manager.history(200);
    let events: Vec<Event> = history.iter().map(|e| e.event).collect();
    for e in [
        Event::Paired,
        Event::Connected,
        Event::Disconnected,
        Event::SwitchedOff,
    ] {
        assert!(events.contains(&e), "{e:?} missing in {events:?}");
    }
    let ok_tool = history
        .iter()
        .find(|e| e.event == Event::Tool && e.ok == Some(true))
        .unwrap();
    assert_eq!(ok_tool.tool.as_deref(), Some("desktop_fake_tool"));
    assert!(history.iter().any(|e| e.event == Event::Tool
        && e.tool.as_deref() == Some("desktop_fail")
        && e.ok == Some(false)));
    let raw = std::fs::read_to_string(dir.path().join("history.jsonl")).unwrap();
    assert!(
        !raw.contains("do-not-log-this"),
        "tool arguments leaked into history"
    );
    assert!(
        !raw.contains("Fake App"),
        "the app name belongs to the live status only"
    );
    manager.shutdown();
}

#[test]
fn stop_ends_the_session_and_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("attach.jsonl");
    let lp = launchpad();
    let (manager, _) = common::open(
        dir.path(),
        common::fake_attach(),
        vec![
            format!("LadOut={}", out.display()),
            "LadHoldMs=60000".into(),
        ],
    );
    let view = common::pair(&manager, &lp.origin);
    assert!(wait(20, || lines(&out) >= 1));
    assert!(wait(10, || matches!(
        state_of(&manager, &view.id),
        State::Connected | State::Working
    )));

    manager.stop(&view.id).unwrap();
    assert!(wait(5, || state_of(&manager, &view.id) == State::Waiting));
    let status = manager.environments()[0].status.clone();
    assert_eq!(status.reason, Some(Reason::Stopped));
    // The switch is still on, so the agent can continue after the pause.
    assert!(
        wait(20, || lines(&out) >= 2),
        "did not reconnect after stop"
    );
    assert!(manager
        .history(100)
        .iter()
        .any(|e| e.event == Event::Disconnected && e.reason.as_deref() == Some("stopped")));
    manager.shutdown();
    assert!(no_key_files(dir.path()));
}

#[test]
fn reports_why_it_cannot_connect() {
    let dir = tempfile::tempdir().unwrap();
    let lp = launchpad();
    let (manager, _) = common::open(
        dir.path(),
        common::fake_attach(),
        vec!["LadReject=1".into()],
    );
    let view = common::pair(&manager, &lp.origin);
    assert!(wait(20, || manager.environments()[0].status.reason
        == Some(Reason::AlreadyConnected)));
    assert_eq!(state_of(&manager, &view.id), State::Waiting);
    assert!(!manager
        .history(100)
        .iter()
        .any(|e| e.event == Event::Connected));
    manager.shutdown();
}

#[test]
fn serves_the_app_tools_next_to_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("attach.jsonl");
    let lp = launchpad();
    let (manager, _) = common::open_full(
        dir.path(),
        common::fake_attach(),
        vec![format!("LadOut={}", out.display()), "LadHoldMs=200".into()],
        common::fake_engine(),
        common::app_tools(),
    );
    common::pair(&manager, &lp.origin);
    assert!(wait(20, || lines(&out) >= 1));
    let first: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    // Engine tools plus the app's own, one answered by the app, not the engine.
    assert_eq!(first["tools"], 3);
    assert_eq!(first["app_status"], "fake-app-status 9.9.9");
    assert_eq!(first["app_meta"]["appVersion"], "9.9.9");
    assert_eq!(first["server"], "fake-engine");
    assert_eq!(first["c1_ok"], true);
    assert!(wait(10, || manager
        .history(100)
        .iter()
        .any(|e| e.event == Event::Tool
            && e.tool.as_deref() == Some("lazurio_app_status")
            && e.ok == Some(true))));
    manager.shutdown();
}
