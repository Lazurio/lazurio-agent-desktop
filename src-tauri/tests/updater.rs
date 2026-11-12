//! The updater against a local `latest.json`, with the same plugin and
//! settings the app uses: a newer release signed with the trusted key is
//! downloaded and verified; a wrong signature, a missing one, a signature for
//! another version and an older release are all refused.
//!
//! Runs where the app builds (macOS, Windows) with a test key generated here.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tauri_plugin_updater::UpdaterExt;

const PACKAGE: &[u8] = b"pretend this is Lazurio Agent Desktop 0.2.0";

struct Key {
    pk: minisign::PublicKey,
    sk: minisign::SecretKey,
}

impl Key {
    fn new() -> Self {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        Self {
            pk: pair.pk,
            sk: pair.sk,
        }
    }

    /// The `pubkey` value of `plugins.updater`: base64 of the minisign key file.
    fn config_value(&self) -> String {
        STANDARD.encode(self.pk.to_box().unwrap().into_string())
    }

    /// A `.sig` as the Tauri CLI writes it: base64 of the minisign signature,
    /// with the signed version in the trusted comment.
    fn sign(&self, data: &[u8], version: &str) -> String {
        let trusted = format!("timestamp:1794528000\tfile:package\tversion:{version}");
        let sig = minisign::sign(
            Some(&self.pk),
            &self.sk,
            std::io::Cursor::new(data),
            Some(&trusted),
            Some("signature from tauri secret key"),
        )
        .unwrap();
        STANDARD.encode(sig.into_string())
    }
}

/// Serves `/latest.json` (swappable) and `/package`.
struct Server {
    base: String,
    manifest: Arc<Mutex<Value>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let manifest = Arc::new(Mutex::new(Value::Null));
        let shared = manifest.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                let (kind, body) = if path.starts_with("/latest.json") {
                    (
                        "application/json",
                        shared.lock().unwrap().to_string().into_bytes(),
                    )
                } else {
                    ("application/octet-stream", PACKAGE.to_vec())
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        Self { base, manifest }
    }

    fn publish(&self, version: &str, signature: &str) {
        let target = tauri_plugin_updater::target().expect("supported target");
        *self.manifest.lock().unwrap() = json!({
            "version": version,
            "notes": "test release",
            "pub_date": "2026-11-12T00:00:00Z",
            "platforms": {
                target: { "signature": signature, "url": format!("{}/package", self.base) }
            }
        });
    }
}

fn updater(trusted: &Key, server: &Server) -> tauri_plugin_updater::Updater {
    let mut context = tauri::test::mock_context(tauri::test::noop_assets());
    context.package_info_mut().version = "0.1.0".parse().unwrap();
    context.config_mut().plugins.0.insert(
        "updater".into(),
        json!({
            "pubkey": trusted.config_value(),
            "endpoints": [format!("{}/latest.json", server.base)],
            // Plain HTTP only for this local test server.
            "dangerousInsecureTransportProtocol": true,
            "requireSignedVersion": true,
        }),
    );
    let app = tauri::test::mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(context)
        .unwrap();
    let exe = std::env::temp_dir()
        .join("Lazurio Agent Desktop.app")
        .join("Contents")
        .join("MacOS")
        .join("lazurio-agent-desktop");
    app.handle()
        .updater_builder()
        .executable_path(exe)
        .build()
        .unwrap()
}

fn check(updater: &tauri_plugin_updater::Updater) -> Option<tauri_plugin_updater::Update> {
    tauri::async_runtime::block_on(updater.check()).unwrap()
}

fn download(update: &tauri_plugin_updater::Update) -> Result<Vec<u8>, String> {
    tauri::async_runtime::block_on(update.download(|_, _| {}, || {})).map_err(|e| e.to_string())
}

#[test]
fn downloads_and_verifies_a_newer_signed_release() {
    let trusted = Key::new();
    let server = Server::start();
    server.publish("0.2.0", &trusted.sign(PACKAGE, "0.2.0"));
    let update = check(&updater(&trusted, &server)).expect("0.2.0 is newer than 0.1.0");
    assert_eq!(update.version, "0.2.0");
    assert_eq!(download(&update).unwrap(), PACKAGE);
}

#[test]
fn refuses_a_package_signed_with_another_key() {
    let trusted = Key::new();
    let stranger = Key::new();
    let server = Server::start();
    server.publish("0.2.0", &stranger.sign(PACKAGE, "0.2.0"));
    let update = check(&updater(&trusted, &server)).expect("announced");
    assert!(download(&update).is_err());
}

#[test]
fn refuses_an_unsigned_package() {
    let trusted = Key::new();
    let server = Server::start();
    server.publish("0.2.0", "");
    let updater = updater(&trusted, &server);
    let refused = match tauri::async_runtime::block_on(updater.check()) {
        Err(_) | Ok(None) => true,
        Ok(Some(update)) => download(&update).is_err(),
    };
    assert!(refused);
}

#[test]
fn refuses_a_signature_made_for_another_version() {
    // An old, genuinely signed package announced as a newer version.
    let trusted = Key::new();
    let server = Server::start();
    server.publish("0.2.0", &trusted.sign(PACKAGE, "0.0.9"));
    let update = check(&updater(&trusted, &server)).expect("announced");
    assert!(download(&update).is_err());
}

#[test]
fn ignores_an_older_or_equal_release() {
    let trusted = Key::new();
    let server = Server::start();
    let updater = updater(&trusted, &server);
    server.publish("0.0.9", &trusted.sign(PACKAGE, "0.0.9"));
    assert!(check(&updater).is_none());
    server.publish("0.1.0", &trusted.sign(PACKAGE, "0.1.0"));
    assert!(check(&updater).is_none());
}
