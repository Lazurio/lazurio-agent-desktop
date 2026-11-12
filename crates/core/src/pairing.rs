//! Pairing with an Environment through its Launchpad (contract v0, section 1).
//!
//! The app listens once on `127.0.0.1:<random port>`, opens the approval page
//! of Launchpad in the system browser and waits until Launchpad redirects the
//! browser back with the same `state` and the result.

use crate::{Error, Result};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const CONNECT_PATH: &str = "/connect/computer";

pub struct PairingRequest {
    /// Normalized Launchpad origin, see [`crate::launchpad::normalize`].
    pub launchpad: String,
    pub computer_name: String,
    pub os: String,
    pub public_key: String,
}

/// Where the computer connects after pairing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    pub host: String,
    pub user: String,
    /// Not part of contract v0, which always uses 22; Launchpad may add it.
    #[serde(default = "default_port")]
    pub port: u16,
    /// `known_hosts` lines for exactly this host and port.
    #[serde(rename = "knownHosts")]
    pub known_hosts: Vec<String>,
}

fn default_port() -> u16 {
    22
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingResult {
    pub device_id: String,
    pub ssh: SshTarget,
    /// The Environment's name as Lazurio shows it (`environment.name`), when
    /// Launchpad sends it. Not part of contract v0; proposed for it.
    pub environment_name: Option<String>,
}

pub struct PairingSession {
    listener: TcpListener,
    state: String,
    url: String,
}

impl PairingSession {
    pub fn start(req: &PairingRequest) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let mut raw = [0u8; 32];
        OsRng.fill_bytes(&mut raw);
        let state = URL_SAFE_NO_PAD.encode(raw);
        let callback = format!("http://127.0.0.1:{port}/cb");
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("name", &req.computer_name)
            .append_pair("os", &req.os)
            .append_pair("pubkey", &req.public_key)
            .append_pair("callback", &callback)
            .append_pair("state", &state)
            .finish();
        let url = format!("{}{}?{}", req.launchpad, CONNECT_PATH, query);
        Ok(Self {
            listener,
            state,
            url,
        })
    }

    /// The approval page to open in the browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits for the redirect from Launchpad. Requests with another `state`
    /// get an error page and do not end the wait.
    pub fn wait(self, timeout: Duration, cancel: &AtomicBool) -> Result<PairingResult> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(Error::PairingCancelled);
            }
            if Instant::now() >= deadline {
                return Err(Error::PairingTimedOut);
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(done) = self.handle(stream) {
                        return done;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn handle(&self, mut stream: TcpStream) -> Option<Result<PairingResult>> {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let Some(target) = read_request_target(&mut stream) else {
            respond(&mut stream, 400, PAGE_INVALID);
            return None;
        };
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        if path != "/cb" {
            respond(&mut stream, 404, PAGE_INVALID);
            return None;
        }
        let mut state = None;
        let mut result = None;
        let mut error = None;
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            match &*k {
                "state" => state = Some(v.into_owned()),
                "result" => result = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                _ => {}
            }
        }
        if !state
            .as_deref()
            .is_some_and(|s| constant_time_eq(s.as_bytes(), self.state.as_bytes()))
        {
            respond(&mut stream, 400, PAGE_INVALID);
            return None;
        }
        if error.is_some() {
            respond(&mut stream, 200, PAGE_DECLINED);
            return Some(Err(Error::PairingDeclined));
        }
        let parsed = result
            .ok_or_else(|| Error::InvalidPairingResult("missing result".into()))
            .and_then(|r| parse_result(&r));
        match &parsed {
            Ok(_) => respond(&mut stream, 200, PAGE_DONE),
            Err(_) => respond(&mut stream, 400, PAGE_FAILED),
        }
        Some(parsed)
    }
}

fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 32 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next()?;
    let mut parts = line.split(' ');
    if parts.next()? != "GET" {
        return None;
    }
    Some(parts.next()?.to_string())
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize)]
struct RawResult {
    #[serde(rename = "deviceId")]
    device_id: String,
    ssh: RawSsh,
    #[serde(default)]
    environment: Option<RawEnvironment>,
}

#[derive(Deserialize)]
struct RawEnvironment {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct RawSsh {
    host: String,
    user: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(rename = "knownHosts")]
    known_hosts: KnownHosts,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum KnownHosts {
    One(String),
    Many(Vec<String>),
}

/// Decodes and checks the `result` parameter. Everything in it ends up on the
/// `ssh` command line or in `known_hosts`, so it is checked strictly.
pub fn parse_result(encoded: &str) -> Result<PairingResult> {
    let trimmed = encoded.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| URL_SAFE.decode(encoded))
        .map_err(|_| Error::InvalidPairingResult("result is not base64url".into()))?;
    let raw: RawResult = serde_json::from_slice(&bytes).map_err(|e| {
        Error::InvalidPairingResult(format!("result is not the expected JSON: {e}"))
    })?;
    if !valid_device_id(&raw.device_id) {
        return Err(Error::InvalidPairingResult("deviceId".into()));
    }
    if !valid_host(&raw.ssh.host) {
        return Err(Error::InvalidPairingResult("ssh.host".into()));
    }
    if !valid_user(&raw.ssh.user) {
        return Err(Error::InvalidPairingResult("ssh.user".into()));
    }
    if raw.ssh.port == 0 {
        return Err(Error::InvalidPairingResult("ssh.port".into()));
    }
    let lines = match raw.ssh.known_hosts {
        KnownHosts::One(s) => s.lines().map(str::to_string).collect::<Vec<_>>(),
        KnownHosts::Many(v) => v,
    };
    let known_hosts = normalize_known_hosts(&raw.ssh.host, raw.ssh.port, &lines);
    if known_hosts.is_empty() {
        return Err(Error::InvalidPairingResult("ssh.knownHosts".into()));
    }
    Ok(PairingResult {
        device_id: raw.device_id,
        ssh: SshTarget {
            host: raw.ssh.host,
            user: raw.ssh.user,
            port: raw.ssh.port,
            known_hosts,
        },
        environment_name: raw
            .environment
            .and_then(|e| e.name)
            .and_then(|n| display_name(&n)),
    })
}

/// A name fit for the panel: control and format characters removed, spaces
/// collapsed, at most 64 characters (the same rule Launchpad uses for names).
pub fn display_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && !is_format_char(*c))
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let name: String = collapsed.chars().take(64).collect();
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn is_format_char(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}')
}

fn valid_device_id(s: &str) -> bool {
    (1..=63).contains(&s.len())
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_host(s: &str) -> bool {
    (1..=253).contains(&s.len())
        && !s.starts_with('-')
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

fn valid_user(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Keeps only the key type and key of each line and writes the host pattern
/// itself, so a line can never pin a key to another host.
pub fn normalize_known_hosts(host: &str, port: u16, lines: &[String]) -> Vec<String> {
    let pattern = if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    let mut out = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(i) = fields.iter().position(|f| is_key_type(f)) else {
            continue;
        };
        let Some(key) = fields.get(i + 1) else {
            continue;
        };
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
        {
            continue;
        }
        let entry = format!("{pattern} {} {key}", fields[i]);
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

fn is_key_type(f: &str) -> bool {
    matches!(
        f,
        "ssh-ed25519"
            | "ssh-rsa"
            | "ecdsa-sha2-nistp256"
            | "ecdsa-sha2-nistp384"
            | "ecdsa-sha2-nistp521"
            | "sk-ssh-ed25519@openssh.com"
            | "sk-ecdsa-sha2-nistp256@openssh.com"
    )
}

const PAGE_DONE: &str = "<!doctype html><meta charset=utf-8><title>Lazurio Agent Desktop</title><body style=\"font:16px system-ui;margin:48px\"><p>Počítač je připojený k Lazuriu. Toto okno můžete zavřít.</p><p lang=en>This computer is connected to Lazurio. You can close this window.</p>";
const PAGE_DECLINED: &str = "<!doctype html><meta charset=utf-8><title>Lazurio Agent Desktop</title><body style=\"font:16px system-ui;margin:48px\"><p>Připojení jste zrušili. Toto okno můžete zavřít.</p><p lang=en>You cancelled the connection. You can close this window.</p>";
const PAGE_FAILED: &str = "<!doctype html><meta charset=utf-8><title>Lazurio Agent Desktop</title><body style=\"font:16px system-ui;margin:48px\"><p>Připojení se nepovedlo. Zkuste to v aplikaci znovu.</p><p lang=en>Connecting failed. Please try again in the app.</p>";
const PAGE_INVALID: &str = "<!doctype html><meta charset=utf-8><title>Lazurio Agent Desktop</title><body style=\"font:16px system-ui;margin:48px\"><p>Neplatný požadavek.</p><p lang=en>Invalid request.</p>";

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(json: &str) -> String {
        URL_SAFE_NO_PAD.encode(json)
    }

    #[test]
    fn parses_the_contract_shape() {
        let r = parse_result(&enc(
            r#"{"deviceId":"office-mac","ssh":{"host":"env.tailnet.example.com","user":"env","knownHosts":"env.tailnet.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB"}}"#,
        ))
        .unwrap();
        assert_eq!(r.device_id, "office-mac");
        assert_eq!(r.ssh.port, 22);
        assert_eq!(r.environment_name, None);
        assert_eq!(
            r.ssh.known_hosts,
            vec!["env.tailnet.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB"]
        );
    }

    #[test]
    fn takes_the_environment_name_when_launchpad_sends_it() {
        let r = parse_result(&enc(
            r#"{"deviceId":"office-mac","environment":{"name":"  Pracovní\u0007  Prostředí\u200b "},"ssh":{"host":"h","user":"u","knownHosts":"ssh-ed25519 AAAA"}}"#,
        ))
        .unwrap();
        assert_eq!(r.environment_name.as_deref(), Some("Pracovní Prostředí"));
        let blank = parse_result(&enc(
            r#"{"deviceId":"x","environment":{"name":" "},"ssh":{"host":"h","user":"u","knownHosts":"ssh-ed25519 AAAA"}}"#,
        ))
        .unwrap();
        assert_eq!(blank.environment_name, None);
        assert_eq!(display_name(&"x".repeat(100)).unwrap().len(), 64);
    }

    #[test]
    fn rewrites_the_host_pattern() {
        let lines = vec![
            "evil.example.com ssh-ed25519 AAAA".to_string(),
            "ssh-rsa BBBB comment".to_string(),
            "@revoked * ssh-ed25519 CCCC".to_string(),
            "ssh-ed25519 bad!key".to_string(),
        ];
        assert_eq!(
            normalize_known_hosts("127.0.0.1", 2222, &lines),
            vec![
                "[127.0.0.1]:2222 ssh-ed25519 AAAA",
                "[127.0.0.1]:2222 ssh-rsa BBBB"
            ]
        );
    }

    #[test]
    fn rejects_option_injection() {
        for json in [
            r#"{"deviceId":"x","ssh":{"host":"-oProxyCommand=x","user":"u","knownHosts":"ssh-ed25519 AAAA"}}"#,
            r#"{"deviceId":"x","ssh":{"host":"h","user":"-l","knownHosts":"ssh-ed25519 AAAA"}}"#,
            r#"{"deviceId":"X Y","ssh":{"host":"h","user":"u","knownHosts":"ssh-ed25519 AAAA"}}"#,
            r#"{"deviceId":"x","ssh":{"host":"h","user":"u","knownHosts":"nothing"}}"#,
        ] {
            assert!(parse_result(&enc(json)).is_err(), "{json}");
        }
        assert!(parse_result("%%%").is_err());
    }

    #[test]
    fn builds_the_approval_url() {
        let s = PairingSession::start(&PairingRequest {
            launchpad: "https://launchpad.acme.example.com".into(),
            computer_name: "Kancelářský Mac".into(),
            os: "macos".into(),
            public_key: "ssh-ed25519 AAAA".into(),
        })
        .unwrap();
        let url = url::Url::parse(s.url()).unwrap();
        assert_eq!(url.path(), CONNECT_PATH);
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["name"], "Kancelářský Mac");
        assert_eq!(q["pubkey"], "ssh-ed25519 AAAA");
        assert!(q["callback"].starts_with("http://127.0.0.1:"));
        assert!(q["callback"].ends_with("/cb"));
        assert_eq!(q["state"].len(), 43);
    }
}
