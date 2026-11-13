//! The connection to the Environment (contract v1, section 2).
//!
//! The computer opens `wss://<launchpad host>/.lazurio/computers/attach` with
//! ordinary certificate checks, answers the Environment's challenge with an
//! Ed25519 signature made in memory with the device key, and after `ready`
//! carries exactly one MCP (JSON-RPC) message per text frame. Nothing on the
//! computer listens on the network, and no key ever touches the disk.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde::Deserialize;
use serde_json::json;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};
use url::{Host, Url};

pub const ATTACH_PATH: &str = "/.lazurio/computers/attach";
/// First line of the signed bytes; the version of the attach handshake.
pub const SIGNING_CONTEXT: &str = "lazurio-computer-attach-v1";

/// A TCP stream, with TLS except for loopback test servers.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref(),
        }
    }

    pub fn set_read_timeout(&self, timeout: Duration) -> std::io::Result<()> {
        self.tcp().set_read_timeout(Some(timeout))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

pub type Socket = WebSocket<Stream>;

#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    /// No route, DNS, timeout, connection reset: try again later.
    Offline(String),
    /// The Environment's certificate did not verify.
    Certificate(String),
    /// The upgrade was answered with this HTTP status (404: no HTTPS channel yet).
    Http(u16),
    /// The Environment refused the computer, with its reason.
    Denied(String),
    /// Something that is not the contract.
    Protocol(String),
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d == "localhost",
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The channel URL for an Environment whose pairing result had none (v0):
/// the Launchpad's host with the attach path.
pub fn derive_channel_url(launchpad_origin: &str) -> Option<String> {
    let mut url = Url::parse(launchpad_origin).ok()?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" if is_loopback(&url) => "ws",
        _ => return None,
    };
    url.set_scheme(scheme).ok()?;
    url.set_path(ATTACH_PATH);
    url.set_query(None);
    Some(url.to_string())
}

/// Checks the `channel.url` of a pairing result: `wss` (plain `ws` only for
/// loopback test servers), the Launchpad's own host, the attach path, and no
/// credentials, query or fragment.
pub fn validate_channel_url(raw: &str, launchpad_origin: &str) -> Result<String, String> {
    let url = Url::parse(raw).map_err(|e| e.to_string())?;
    let launchpad = Url::parse(launchpad_origin).map_err(|e| e.to_string())?;
    match url.scheme() {
        "wss" => {}
        "ws" if is_loopback(&url) => {}
        other => return Err(format!("scheme {other} is not allowed")),
    }
    if url.host_str() != launchpad.host_str() {
        return Err("the channel is not on the Launchpad's host".into());
    }
    if url.path() != ATTACH_PATH {
        return Err("unexpected channel path".into());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("unexpected parts in the channel URL".into());
    }
    Ok(url.to_string())
}

fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

fn io_error(e: std::io::Error) -> WireError {
    if let Some(tls) = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        if matches!(
            tls,
            rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented
        ) {
            return WireError::Certificate(tls.to_string());
        }
    }
    WireError::Offline(e.to_string())
}

fn ws_error(e: tungstenite::Error) -> WireError {
    match e {
        tungstenite::Error::Io(io) => io_error(io),
        tungstenite::Error::Http(response) => WireError::Http(response.status().as_u16()),
        other => WireError::Offline(other.to_string()),
    }
}

/// Opens the WebSocket, with TLS for `wss`.
pub fn connect(url: &Url, timeout: Duration) -> Result<Socket, WireError> {
    let host = url
        .host_str()
        .ok_or_else(|| WireError::Protocol("no host".into()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let tls = match url.scheme() {
        "wss" => true,
        "ws" if is_loopback(url) => false,
        other => return Err(WireError::Protocol(format!("scheme {other}"))),
    };
    let port = url
        .port_or_known_default()
        .unwrap_or(if tls { 443 } else { 80 });
    let addrs: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| WireError::Offline(e.to_string()))?
        .collect();
    let mut last = WireError::Offline("no address".into());
    let mut tcp = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last = WireError::Offline(e.to_string()),
        }
    }
    let tcp = tcp.ok_or(last)?;
    let _ = tcp.set_nodelay(true);
    tcp.set_read_timeout(Some(timeout)).map_err(io_error)?;
    tcp.set_write_timeout(Some(timeout)).map_err(io_error)?;
    let stream = if tls {
        let name = match host.parse::<IpAddr>() {
            Ok(ip) => ServerName::IpAddress(ip.into()),
            Err(_) => ServerName::try_from(host.clone())
                .map_err(|e| WireError::Protocol(e.to_string()))?,
        };
        let conn = ClientConnection::new(tls_config(), name)
            .map_err(|e| WireError::Protocol(e.to_string()))?;
        Stream::Tls(Box::new(StreamOwned::new(conn, tcp)))
    } else {
        Stream::Plain(tcp)
    };
    match tungstenite::client::client(url.as_str(), stream) {
        Ok((socket, _response)) => Ok(socket),
        Err(tungstenite::HandshakeError::Failure(e)) => Err(ws_error(e)),
        Err(tungstenite::HandshakeError::Interrupted(_)) => {
            Err(WireError::Offline("handshake timed out".into()))
        }
    }
}

/// The exact bytes the device signs.
pub fn signing_bytes(environment: &str, nonce: &str, ts: i64) -> Vec<u8> {
    format!("{SIGNING_CONTEXT}\n{environment}\n{nonce}\n{ts}").into_bytes()
}

/// Ed25519 signature with the device seed, base64url without padding.
pub fn sign(seed: &[u8; 32], message: &[u8]) -> String {
    let key = SigningKey::from_bytes(seed);
    URL_SAFE_NO_PAD.encode(key.sign(message).to_bytes())
}

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// Reads the next text frame before `deadline`.
pub fn read_text(socket: &mut Socket, deadline: Instant) -> Result<String, WireError> {
    loop {
        if Instant::now() >= deadline {
            return Err(WireError::Offline("no answer in time".into()));
        }
        match socket.read() {
            Ok(Message::Text(t)) => return Ok(t.to_string()),
            Ok(Message::Close(_)) => return Err(WireError::Offline("closed".into())),
            Ok(_) => {}
            Err(e) if is_timeout(&e) => {}
            Err(e) => return Err(ws_error(e)),
        }
    }
}

#[derive(Deserialize)]
struct Challenge {
    #[serde(rename = "type")]
    kind: String,
    v: u32,
    environment: String,
    nonce: String,
    ts: i64,
}

#[derive(Deserialize)]
struct Verdict {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    reason: Option<String>,
}

/// Answers the challenge and waits for `ready`.
pub fn authenticate(
    socket: &mut Socket,
    device_id: &str,
    seed: &[u8; 32],
    timeout: Duration,
) -> Result<(), WireError> {
    let deadline = Instant::now() + timeout;
    let raw = read_text(socket, deadline)?;
    let challenge: Challenge =
        serde_json::from_str(&raw).map_err(|e| WireError::Protocol(format!("challenge: {e}")))?;
    if challenge.kind == "denied" {
        let verdict: Verdict = serde_json::from_str(&raw).unwrap_or(Verdict {
            kind: "denied".into(),
            reason: None,
        });
        return Err(WireError::Denied(verdict.reason.unwrap_or_default()));
    }
    if challenge.kind != "challenge" || challenge.v != 1 {
        return Err(WireError::Protocol(format!(
            "expected challenge v1, got {}",
            challenge.kind
        )));
    }
    let signature = sign(
        seed,
        &signing_bytes(&challenge.environment, &challenge.nonce, challenge.ts),
    );
    let auth = json!({"type": "auth", "v": 1, "device": device_id, "signature": signature});
    socket
        .send(Message::text(auth.to_string()))
        .map_err(ws_error)?;
    let raw = read_text(socket, deadline)?;
    let verdict: Verdict =
        serde_json::from_str(&raw).map_err(|e| WireError::Protocol(format!("verdict: {e}")))?;
    match verdict.kind.as_str() {
        "ready" => Ok(()),
        "denied" => Err(WireError::Denied(verdict.reason.unwrap_or_default())),
        other => Err(WireError::Protocol(format!("expected ready, got {other}"))),
    }
}

/// The URL with `?device=<id>`.
pub fn attach_url(channel_url: &str, device_id: &str) -> Result<Url, WireError> {
    let mut url = Url::parse(channel_url).map_err(|e| WireError::Protocol(e.to_string()))?;
    url.query_pairs_mut()
        .clear()
        .append_pair("device", device_id);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    #[test]
    fn signs_the_contract_bytes() {
        let seed = [7u8; 32];
        let msg = signing_bytes("env-1", "bm9uY2U", 1_794_528_000_000);
        assert_eq!(
            String::from_utf8(msg.clone()).unwrap(),
            "lazurio-computer-attach-v1\nenv-1\nbm9uY2U\n1794528000000"
        );
        let sig = URL_SAFE_NO_PAD.decode(sign(&seed, &msg)).unwrap();
        let key = SigningKey::from_bytes(&seed).verifying_key();
        key.verify(&msg, &Signature::from_slice(&sig).unwrap())
            .unwrap();
    }

    #[test]
    fn derives_and_validates_channel_urls() {
        assert_eq!(
            derive_channel_url("https://launchpad.acme.example.com").unwrap(),
            "wss://launchpad.acme.example.com/.lazurio/computers/attach"
        );
        assert_eq!(
            derive_channel_url("http://127.0.0.1:8123").unwrap(),
            "ws://127.0.0.1:8123/.lazurio/computers/attach"
        );
        let lp = "https://launchpad.acme.example.com";
        assert!(validate_channel_url(
            "wss://launchpad.acme.example.com/.lazurio/computers/attach",
            lp
        )
        .is_ok());
        for bad in [
            "ws://launchpad.acme.example.com/.lazurio/computers/attach",
            "wss://evil.example.com/.lazurio/computers/attach",
            "wss://launchpad.acme.example.com/other",
            "wss://u:p@launchpad.acme.example.com/.lazurio/computers/attach",
            "wss://launchpad.acme.example.com/.lazurio/computers/attach?x=1",
        ] {
            assert!(validate_channel_url(bad, lp).is_err(), "{bad}");
        }
        let url = attach_url(
            "wss://launchpad.acme.example.com/.lazurio/computers/attach",
            "office-mac",
        )
        .unwrap();
        assert_eq!(url.query(), Some("device=office-mac"));
    }
}
