//! Pairing with an Environment through its Launchpad (contract v0, section 1).
//!
//! The app listens once on `127.0.0.1:<random port>`, opens the approval page
//! of Launchpad in the system browser and waits until Launchpad redirects the
//! browser back with the same `state` and the result.

use crate::{Error, Result};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingResult {
    pub device_id: String,
    /// `wss://<launchpad host>/.lazurio/computers/attach` (contract v1).
    pub channel_url: String,
    /// The Environment's name as Lazurio shows it (`environment.name`), when
    /// Launchpad sends it.
    pub environment_name: Option<String>,
}

pub struct PairingSession {
    listener: TcpListener,
    state: String,
    url: String,
    launchpad: String,
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
            launchpad: req.launchpad.clone(),
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
            .and_then(|r| parse_result(&r, &self.launchpad));
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
    #[serde(default)]
    channel: Option<RawChannel>,
    /// v0: an SSH target. Only its presence matters now: the channel is then
    /// derived from the Launchpad's address.
    #[serde(default)]
    ssh: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    environment: Option<RawEnvironment>,
}

#[derive(Deserialize)]
struct RawChannel {
    url: String,
}

#[derive(Deserialize)]
struct RawEnvironment {
    #[serde(default)]
    name: Option<String>,
}

/// Decodes and checks the `result` parameter. The channel must be a `wss`
/// URL on the Launchpad's own host; a v0 result (`ssh`) gets its channel from
/// the Launchpad's address.
pub fn parse_result(encoded: &str, launchpad_origin: &str) -> Result<PairingResult> {
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
    let channel_url = match (raw.channel, raw.ssh) {
        (Some(channel), _) => crate::wire::validate_channel_url(&channel.url, launchpad_origin)
            .map_err(|e| Error::InvalidPairingResult(format!("channel.url: {e}")))?,
        (None, Some(_)) => crate::wire::derive_channel_url(launchpad_origin)
            .ok_or_else(|| Error::InvalidPairingResult("no channel for this Launchpad".into()))?,
        (None, None) => return Err(Error::InvalidPairingResult("channel".into())),
    };
    Ok(PairingResult {
        device_id: raw.device_id,
        channel_url,
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

const PAGE_DONE: &str = "<!doctype html><meta charset=utf-8><title>Lazurio Agent Desktop</title><body style=\"font:16px system-ui;margin:48px\"><p>Počítač je připojený k Lazuriu. Toto okno můžete zavřít.</p><p lang=en>This computer is connected to Lazurio. You can close this window.</p>";
const PAGE_DECLINED: &str = "<!doctype html><meta charset=utf-8><title>Lazurio Agent Desktop</title><body style=\"font:16px system-ui;margin:48px\"><p>Připojení jste zrušili. Toto okno můžete zavřít.</p><p lang=en>You cancelled the connection. You can close this window.</p>";
const PAGE_FAILED: &str = "<!doctype html><meta charset=utf-8><title>Lazurio Agent Desktop</title><body style=\"font:16px system-ui;margin:48px\"><p>Připojení se nepovedlo. Zkuste to v aplikaci znovu.</p><p lang=en>Connecting failed. Please try again in the app.</p>";
const PAGE_INVALID: &str = "<!doctype html><meta charset=utf-8><title>Lazurio Agent Desktop</title><body style=\"font:16px system-ui;margin:48px\"><p>Neplatný požadavek.</p><p lang=en>Invalid request.</p>";

#[cfg(test)]
mod tests {
    use super::*;

    const LP: &str = "https://launchpad.acme.example.com";

    fn enc(json: &str) -> String {
        URL_SAFE_NO_PAD.encode(json)
    }

    #[test]
    fn parses_the_v1_shape() {
        let r = parse_result(
            &enc(r#"{"deviceId":"office-mac","channel":{"url":"wss://launchpad.acme.example.com/.lazurio/computers/attach"},"environment":{"name":"  Pracovní\u0007  Prostředí\u200b "}}"#),
            LP,
        )
        .unwrap();
        assert_eq!(r.device_id, "office-mac");
        assert_eq!(
            r.channel_url,
            "wss://launchpad.acme.example.com/.lazurio/computers/attach"
        );
        assert_eq!(r.environment_name.as_deref(), Some("Pracovní Prostředí"));
        assert_eq!(display_name(&"x".repeat(100)).unwrap().len(), 64);
    }

    #[test]
    fn derives_the_channel_for_a_v0_result() {
        let r = parse_result(
            &enc(r#"{"deviceId":"office-mac","ssh":{"host":"h","user":"u","knownHosts":"ssh-ed25519 AAAA"}}"#),
            LP,
        )
        .unwrap();
        assert_eq!(
            r.channel_url,
            "wss://launchpad.acme.example.com/.lazurio/computers/attach"
        );
        assert_eq!(r.environment_name, None);
    }

    #[test]
    fn rejects_foreign_channels_and_bad_ids() {
        for json in [
            r#"{"deviceId":"x","channel":{"url":"wss://evil.example.com/.lazurio/computers/attach"}}"#,
            r#"{"deviceId":"x","channel":{"url":"ws://launchpad.acme.example.com/.lazurio/computers/attach"}}"#,
            r#"{"deviceId":"X Y","channel":{"url":"wss://launchpad.acme.example.com/.lazurio/computers/attach"}}"#,
            r#"{"deviceId":"x"}"#,
        ] {
            assert!(parse_result(&enc(json), LP).is_err(), "{json}");
        }
        assert!(parse_result("%%%", LP).is_err());
    }

    #[test]
    fn builds_the_approval_url() {
        let s = PairingSession::start(&PairingRequest {
            launchpad: LP.into(),
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
