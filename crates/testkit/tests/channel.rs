//! The channel over WebSocket against a fake Environment (contract v1).

mod common;

use common::{first_line, lines, state_of, wait, Setup};
use lazurio_agent_desktop_core::channel::{Reason, State};
use lazurio_agent_desktop_core::history::Event;
use lazurio_agent_desktop_testkit::{EnvOptions, FakeEnvironment};
use std::time::Duration;

#[test]
fn serves_mcp_reconnects_and_obeys_the_switch() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnvironment::start(EnvOptions::default(), dir.path().join("agent.jsonl"));
    let lp = common::launchpad_for(&env, "test-computer", None);
    let (manager, _) = common::open(dir.path(), Setup::default());
    let view = common::pair(&manager, &lp.origin);
    assert!(view.enabled);

    // The Environment ends each session; with the switch on the app connects again.
    assert!(wait(30, || lines(&env.out) >= 2), "no reconnect");
    let first = first_line(&env.out);
    assert_eq!(first["server"], "fake-engine");
    assert_eq!(first["ping"], true);
    assert_eq!(first["tools"], 2);
    assert_eq!(first["c1_ok"], true);
    assert_eq!(first["c2_error"], true);

    manager.set_enabled(&view.id, false).unwrap();
    assert!(wait(10, || state_of(&manager, &view.id) == State::Off));
    let sessions = env.stats().sessions;
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        env.stats().sessions,
        sessions,
        "connected while switched off"
    );

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
    assert!(history.iter().any(|e| e.event == Event::Tool
        && e.tool.as_deref() == Some("desktop_fake_tool")
        && e.ok == Some(true)));
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
fn reconnects_when_the_environment_stops_answering_pings() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnvironment::start(
        EnvOptions {
            silent_after_ready: true,
            ..EnvOptions::default()
        },
        dir.path().join("agent.jsonl"),
    );
    let lp = common::launchpad_for(&env, "test-computer", None);
    let (manager, _) = common::open(dir.path(), Setup::default());
    common::pair(&manager, &lp.origin);
    // No pong within dead_after (1 s in tests): offline, then a new session.
    assert!(wait(20, || env.stats().sessions >= 2), "{:?}", env.stats());
    manager.shutdown();
}

#[test]
fn pings_while_the_environment_holds_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnvironment::start(
        EnvOptions {
            hold: Duration::from_secs(2),
            ..EnvOptions::default()
        },
        dir.path().join("agent.jsonl"),
    );
    let lp = common::launchpad_for(&env, "test-computer", None);
    let (manager, _) = common::open(dir.path(), Setup::default());
    common::pair(&manager, &lp.origin);
    assert!(wait(20, || lines(&env.out) >= 1));
    // Every 200 ms in tests (15 s in the app).
    assert!(wait(5, || env.stats().pings >= 3), "{:?}", env.stats());
    manager.shutdown();
}

#[test]
fn stop_ends_the_session_and_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnvironment::start(
        EnvOptions {
            hold: Duration::from_secs(60),
            ..EnvOptions::default()
        },
        dir.path().join("agent.jsonl"),
    );
    let lp = common::launchpad_for(&env, "test-computer", None);
    let (manager, _) = common::open(dir.path(), Setup::default());
    let view = common::pair(&manager, &lp.origin);
    assert!(wait(20, || lines(&env.out) >= 1));
    assert!(wait(10, || matches!(
        state_of(&manager, &view.id),
        State::Connected | State::Working
    )));

    manager.stop(&view.id).unwrap();
    assert!(wait(5, || state_of(&manager, &view.id) == State::Waiting));
    assert_eq!(
        manager.environments()[0].status.reason,
        Some(Reason::Stopped)
    );
    // The switch is still on, so the agent can continue after the pause.
    assert!(
        wait(20, || lines(&env.out) >= 2),
        "did not reconnect after stop"
    );
    assert!(manager
        .history(100)
        .iter()
        .any(|e| e.event == Event::Disconnected && e.reason.as_deref() == Some("stopped")));
    manager.shutdown();
}

#[test]
fn reports_why_the_environment_refused() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnvironment::start(
        EnvOptions {
            deny: Some("removed".into()),
            ..EnvOptions::default()
        },
        dir.path().join("agent.jsonl"),
    );
    let lp = common::launchpad_for(&env, "test-computer", None);
    let (manager, _) = common::open(dir.path(), Setup::default());
    let view = common::pair(&manager, &lp.origin);
    assert!(wait(20, || manager.environments()[0].status.reason
        == Some(Reason::NotAuthorized)));
    assert_eq!(state_of(&manager, &view.id), State::Waiting);
    assert_eq!(lines(&env.out), 0);
    manager.shutdown();
}

#[test]
fn serves_the_app_tools_next_to_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnvironment::start(EnvOptions::default(), dir.path().join("agent.jsonl"));
    let lp = common::launchpad_for(&env, "test-computer", None);
    let (manager, _) = common::open(
        dir.path(),
        Setup {
            app_tools: common::app_tools(),
            ..Setup::default()
        },
    );
    common::pair(&manager, &lp.origin);
    assert!(wait(20, || lines(&env.out) >= 1));
    let first = first_line(&env.out);
    // Engine tools plus the app's own, one answered by the app, not the engine.
    assert_eq!(first["tools"], 3);
    assert_eq!(first["app_status"], "fake-app-status 9.9.9");
    assert_eq!(first["app_meta"]["appVersion"], "9.9.9");
    assert_eq!(first["server"], "fake-engine");
    assert!(wait(10, || manager
        .history(100)
        .iter()
        .any(|e| e.event == Event::Tool
            && e.tool.as_deref() == Some("lazurio_app_status")
            && e.ok == Some(true))));
    manager.shutdown();
}

#[test]
fn one_agent_at_a_time_across_environments() {
    let dir = tempfile::tempdir().unwrap();
    // Environment A's agent works first and keeps its session; Environment B's agent tries a second later.
    let first = FakeEnvironment::start(
        EnvOptions {
            environment: "env-a".into(),
            hold: Duration::from_secs(10),
            ..EnvOptions::default()
        },
        dir.path().join("a.jsonl"),
    );
    let second = FakeEnvironment::start(
        EnvOptions {
            environment: "env-b".into(),
            call_delay: Duration::from_secs(1),
            hold: Duration::from_secs(10),
            ..EnvOptions::default()
        },
        dir.path().join("b.jsonl"),
    );
    let lp1 = common::launchpad_for(&first, "test-computer", Some("Prostředí A"));
    let lp2 = common::launchpad_for(&second, "test-computer", Some("Prostředí B"));
    let (manager, _) = common::open(dir.path(), Setup::default());
    let a = common::pair(&manager, &lp1.origin);
    assert!(wait(20, || lines(&first.out) >= 1));
    let b = common::pair(&manager, &lp2.origin);
    assert!(wait(20, || lines(&second.out) >= 1));

    let refused = first_line(&second.out);
    assert_eq!(refused["c1_ok"], false);
    let text = refused["c1_text"]
        .as_str()
        .unwrap_or_else(|| panic!("{refused}"));
    assert!(text.contains("ovládá agent z Prostředí A"), "{text}");
    // The panel tells Environment B who works now; Environment A itself sees nobody else.
    let views = manager.environments();
    let view_a = views.iter().find(|e| e.id == a.id).unwrap();
    let view_b = views.iter().find(|e| e.id == b.id).unwrap();
    assert_eq!(view_b.controlled_by.as_deref(), Some("Prostředí A"));
    assert_eq!(view_a.controlled_by, None);
    assert_eq!(manager.controlled_by().unwrap().label, "Prostředí A");
    manager.shutdown();
}

#[test]
fn runs_commands_only_after_the_owner_turns_them_on() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnvironment::start(
        EnvOptions {
            shell_command: Some("echo hi-from-the-shell".into()),
            ..EnvOptions::default()
        },
        dir.path().join("agent.jsonl"),
    );
    let lp = common::launchpad_for(&env, "test-computer", None);
    let (manager, _) = common::open(dir.path(), Setup::default());
    let view = common::pair(&manager, &lp.origin);
    assert!(!view.commands, "commands start off");
    assert!(wait(20, || lines(&env.out) >= 1));
    assert_eq!(first_line(&env.out)["tools"], 2, "no shell tools while off");

    manager.set_commands(&view.id, true).unwrap();
    let ran = || {
        std::fs::read_to_string(&env.out)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|l| l["shell"].is_object())
    };
    assert!(
        wait(30, || ran().is_some()),
        "no command ran after switching on"
    );
    let line = ran().unwrap();
    assert_eq!(line["tools"], 7);
    assert_eq!(line["shell"]["exitCode"], 0);
    assert_eq!(
        line["shell"]["stdout"].as_str().unwrap().trim(),
        "hi-from-the-shell"
    );

    let history = manager.history(200);
    assert!(history.iter().any(|e| e.event == Event::CommandsOn));
    let cmd = history.iter().find(|e| e.event == Event::Command).unwrap();
    assert_eq!(cmd.command.as_deref(), Some("echo hi-from-the-shell"));
    assert_eq!(cmd.exit_code, Some(0));
    let raw = std::fs::read_to_string(dir.path().join("history.jsonl")).unwrap();
    assert_eq!(
        raw.matches("hi-from-the-shell").count(),
        1,
        "the command once, its output never"
    );
    manager.shutdown();
}

/// The real Agent Desktop engine over the WebSocket channel, when `LAD_ENGINE`
/// is set (`scripts/build-engine.sh` or a local build of the fork).
#[test]
fn real_engine_over_the_channel() {
    let Some(engine) = common::real_engine() else {
        eprintln!("skipping: LAD_ENGINE is not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnvironment::start(EnvOptions::default(), dir.path().join("agent.jsonl"));
    let lp = common::launchpad_for(&env, "test-computer", None);
    let (manager, _) = common::open(
        dir.path(),
        Setup {
            engine,
            ..Setup::default()
        },
    );
    let view = common::pair(&manager, &lp.origin);
    assert!(
        wait(60, || lines(&env.out) >= 1),
        "no MCP session with the real engine: {:?}",
        manager.environments()[0].status
    );
    let first = first_line(&env.out);
    assert_eq!(first["server"], "agent-desktop", "{first}");
    assert_eq!(
        first["ping"], true,
        "the broker's liveness ping needs an answer"
    );
    assert!(first["tools"].as_u64().unwrap() > 10, "{first}");
    manager.set_enabled(&view.id, false).unwrap();
    assert!(wait(10, || state_of(&manager, &view.id) == State::Off));
    manager.shutdown();
}
