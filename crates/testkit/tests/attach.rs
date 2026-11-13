//! The attach handshake of contract v1 against the fake Environment: what the
//! computer signs, and what the Environment refuses.

use lazurio_agent_desktop_core::keys::DeviceKey;
use lazurio_agent_desktop_core::wire::{self, WireError};
use lazurio_agent_desktop_testkit::{EnvOptions, FakeEnvironment};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

fn seed_of(key: &DeviceKey) -> [u8; 32] {
    key.seed().as_slice().try_into().unwrap()
}

fn attach(env: &FakeEnvironment, device: &str, seed: &[u8; 32]) -> Result<wire::Socket, WireError> {
    let url = wire::attach_url(&env.channel_url, device).unwrap();
    let mut socket = wire::connect(&url, TIMEOUT)?;
    wire::authenticate(&mut socket, device, seed, TIMEOUT)?;
    Ok(socket)
}

fn env_with(options: EnvOptions) -> (tempfile::TempDir, FakeEnvironment, DeviceKey) {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnvironment::start(
        EnvOptions {
            silent_after_ready: true,
            ..options
        },
        dir.path().join("agent.jsonl"),
    );
    let key = DeviceKey::generate().unwrap();
    env.registry.register("office-mac", &key.public_openssh());
    (dir, env, key)
}

#[test]
fn a_valid_signature_gets_ready() {
    let (_dir, env, key) = env_with(EnvOptions::default());
    attach(&env, "office-mac", &seed_of(&key)).expect("ready");
    assert_eq!(env.stats().sessions, 1);
}

#[test]
fn a_foreign_key_is_refused() {
    let (_dir, env, _key) = env_with(EnvOptions::default());
    let stranger = DeviceKey::generate().unwrap();
    let err = attach(&env, "office-mac", &seed_of(&stranger))
        .err()
        .unwrap();
    assert_eq!(err, WireError::Denied("bad-signature".into()));
}

#[test]
fn an_unknown_device_is_refused() {
    let (_dir, env, key) = env_with(EnvOptions::default());
    env.registry.remove("office-mac");
    let err = attach(&env, "office-mac", &seed_of(&key)).err().unwrap();
    assert_eq!(err, WireError::Denied("unknown-device".into()));
}

#[test]
fn an_old_challenge_is_refused() {
    let (_dir, env, key) = env_with(EnvOptions {
        ts_offset_ms: -120_000,
        ..EnvOptions::default()
    });
    let err = attach(&env, "office-mac", &seed_of(&key)).err().unwrap();
    assert_eq!(err, WireError::Denied("stale-challenge".into()));
}

#[test]
fn a_repeated_nonce_is_refused() {
    let (_dir, env, key) = env_with(EnvOptions {
        fixed_nonce: Some("c2FtZS1ub25jZS1ldmVyeS10aW1lLXNhbWUtbm9uY2U".into()),
        ..EnvOptions::default()
    });
    let first = attach(&env, "office-mac", &seed_of(&key)).expect("first use of the nonce");
    drop(first);
    std::thread::sleep(Duration::from_millis(200));
    let err = attach(&env, "office-mac", &seed_of(&key)).err().unwrap();
    assert_eq!(err, WireError::Denied("replayed-nonce".into()));
}

#[test]
fn a_second_session_of_the_same_computer_is_refused() {
    let (_dir, env, key) = env_with(EnvOptions::default());
    let _first = attach(&env, "office-mac", &seed_of(&key)).expect("ready");
    let err = attach(&env, "office-mac", &seed_of(&key)).err().unwrap();
    assert_eq!(err, WireError::Denied("already-attached".into()));
}

#[test]
fn a_missing_endpoint_is_reported_as_such() {
    let (_dir, env, key) = env_with(EnvOptions::default());
    let url = url::Url::parse(
        &env.channel_url
            .replace("/.lazurio/computers/attach", "/other"),
    )
    .unwrap();
    let err = wire::connect(&url, TIMEOUT).err().unwrap();
    assert_eq!(err, WireError::Http(404));
    let _ = key;
}
