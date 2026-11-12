//! Pairing and channel over the real OpenSSH client against a local `sshd`.
//!
//! The fake Launchpad writes the contract's `authorized_keys` line with
//! `restrict,command="…"`, the forced command is `fake-attach`, and the app
//! connects with the exact command line it uses in production.
//!
//! Needs `/usr/sbin/sshd` and `/usr/bin/ssh`; skipped when they are missing,
//! unless `LAD_REQUIRE_SSHD=1` (CI on Linux) makes that a failure.

#![cfg(unix)]

mod common;

use common::{lines, no_key_files, state_of, wait};
use lazurio_agent_desktop_core::channel::{Reason, State};
use lazurio_agent_desktop_core::history::Event;
use lazurio_agent_desktop_testkit::{Approval, FakeLaunchpad};
use rand_core::OsRng;
use ssh_key::{Algorithm, LineEnding, PrivateKey};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const SSHD: &str = "/usr/sbin/sshd";
const SSH: &str = "/usr/bin/ssh";

struct Sshd {
    child: Child,
    port: u16,
    host_public: String,
    log: PathBuf,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn available() -> bool {
    let ok = Path::new(SSHD).exists() && Path::new(SSH).exists();
    if !ok && std::env::var("LAD_REQUIRE_SSHD").as_deref() == Ok("1") {
        panic!("sshd test required but {SSHD} or {SSH} is missing");
    }
    if !ok {
        eprintln!("skipping: {SSHD} or {SSH} is missing");
    }
    ok
}

fn user() -> String {
    std::env::var("USER").unwrap_or_else(|_| {
        String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string()
    })
}

fn start_sshd(dir: &Path) -> Sshd {
    let host_key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let host_key_path = dir.join("host_ed25519");
    std::fs::write(
        &host_key_path,
        host_key.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    std::fs::set_permissions(&host_key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let host_public = host_key.public_key().to_openssh().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = dir.join("sshd_config");
    std::fs::write(
        &config,
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\n\
             StrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
             PubkeyAuthentication yes\nLogLevel VERBOSE\n",
            host_key_path.display(),
            dir.join("sshd.pid").display(),
            dir.join("authorized_keys").display(),
        ),
    )
    .unwrap();
    std::fs::write(dir.join("authorized_keys"), "").unwrap();
    let log = dir.join("sshd.log");
    let child = Command::new(SSHD)
        .args(["-D", "-e", "-f"])
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    assert!(
        wait(10, || std::net::TcpStream::connect(("127.0.0.1", port))
            .is_ok()),
        "sshd did not start: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    Sshd {
        child,
        port,
        host_public,
        log,
    }
}

fn launchpad(sshd: &Sshd, dir: &Path, out: &Path, known_hosts: String) -> FakeLaunchpad {
    let authorized = dir.join("authorized_keys");
    let attach = common::fake_attach();
    let out = out.to_path_buf();
    FakeLaunchpad::start(Approval {
        device_id: "test-computer".into(),
        host: "127.0.0.1".into(),
        user: user(),
        port: sshd.port,
        known_hosts,
        decline: false,
        environment_name: None,
        on_approve: Some(Box::new(move |q, device| {
            // The line Launchpad writes (contract v0 with the absolute path of
            // `lazurio`; here the forced command is the fake attach).
            let line = format!(
                "restrict,command=\"{} --out {}\" {} lazurio-computer:{device}:test:2026-11-12\n",
                attach.display(),
                out.display(),
                q["pubkey"]
            );
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&authorized)
                .unwrap();
            std::io::Write::write_all(&mut f, line.as_bytes()).unwrap();
        })),
    })
}

#[test]
fn pairs_and_serves_mcp_over_real_openssh() {
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // A space in the path, as in "Application Support" on macOS.
    let dir = tmp.path().join("data dir");
    std::fs::create_dir_all(&dir).unwrap();
    let sshd = start_sshd(tmp.path());
    let out = tmp.path().join("attach.jsonl");
    // Launchpad sends the host key without a host name; the app adds `[127.0.0.1]:port`.
    let lp = launchpad(&sshd, tmp.path(), &out, sshd.host_public.clone());
    let (manager, _) = common::open(&dir, PathBuf::from(SSH), vec![]);
    let view = common::pair(&manager, &lp.origin);

    let connected = wait(30, || lines(&out) >= 1);
    if !connected {
        panic!(
            "no MCP session over ssh. status: {:?}\nsshd log:\n{}",
            manager.environments()[0].status,
            std::fs::read_to_string(&sshd.log).unwrap_or_default()
        );
    }
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

    let known = std::fs::read_to_string(dir.join("known_hosts").join(&view.id)).unwrap();
    assert!(known.starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", sshd.port)));

    manager.set_enabled(&view.id, false).unwrap();
    assert!(wait(10, || state_of(&manager, &view.id) == State::Off));
    assert!(no_key_files(&dir));
    let history = manager.history(100);
    assert!(history.iter().any(|e| e.event == Event::Connected));
    assert!(history
        .iter()
        .any(|e| e.event == Event::Tool && e.tool.as_deref() == Some("desktop_fake_tool")));
    manager.shutdown();
}

#[test]
fn refuses_a_host_with_another_key() {
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let sshd = start_sshd(tmp.path());
    let out = tmp.path().join("attach.jsonl");
    let other = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .unwrap()
        .public_key()
        .to_openssh()
        .unwrap();
    let lp = launchpad(&sshd, tmp.path(), &out, other);
    let (manager, _) = common::open(&dir, PathBuf::from(SSH), vec![]);
    common::pair(&manager, &lp.origin);
    assert!(
        wait(30, || manager.environments()[0].status.reason
            == Some(Reason::HostKey)),
        "status: {:?}",
        manager.environments()[0].status
    );
    assert_eq!(lines(&out), 0);
    manager.shutdown();
    assert!(no_key_files(&dir));
}

/// The real Agent Desktop engine over real OpenSSH, when `LAD_ENGINE` is set
/// (`scripts/build-engine.sh` or a local build of the fork).
#[test]
fn real_engine_over_real_openssh() {
    let Some(engine) = common::real_engine() else {
        eprintln!("skipping: LAD_ENGINE is not set");
        return;
    };
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let sshd = start_sshd(tmp.path());
    let out = tmp.path().join("attach.jsonl");
    let lp = launchpad(&sshd, tmp.path(), &out, sshd.host_public.clone());
    let (manager, _) = common::open_with_engine(&dir, PathBuf::from(SSH), vec![], engine);
    let view = common::pair(&manager, &lp.origin);
    assert!(
        wait(60, || lines(&out) >= 1),
        "no MCP session with the real engine: {:?}",
        manager.environments()[0].status
    );
    let first: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
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
