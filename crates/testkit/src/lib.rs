//! Fakes of the Environment side for the core's end-to-end tests.
//!
//! - [`FakeLaunchpad`]: `GET /connect/computer?…` acts as if the owner had
//!   signed in and pressed "Allow agents on this computer". It registers the
//!   computer's public key and redirects the browser to the app's callback
//!   with the contract's `result`.
//! - [`FakeEnvironment`]: the attach endpoint of contract v1 as a WebSocket
//!   server. It sends the challenge, checks the Ed25519 signature against the
//!   registered key, the age of `ts`, that a `nonce` is used once and that a
//!   computer has one session, answers `ready` or `denied`, and then plays an
//!   agent: initialize, ping, tools/list, the app's status tool when listed,
//!   one tool call that succeeds and one that fails. Each session appends a
//!   summary line to its out file.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::{Message, WebSocket};

pub type Query = HashMap<String, String>;

/// Device id → Ed25519 public key, as the real Launchpad keeps in its list of
/// computers.
#[derive(Default)]
pub struct Registry(Mutex<HashMap<String, [u8; 32]>>);

impl Registry {
    /// Registers the `ssh-ed25519 …` key the app sent in `pubkey`.
    pub fn register(&self, device_id: &str, openssh: &str) {
        let key = ssh_key::PublicKey::from_openssh(openssh).expect("an OpenSSH public key");
        let ed = key.key_data().ed25519().expect("ed25519").0;
        self.0.lock().unwrap().insert(device_id.to_string(), ed);
    }

    pub fn remove(&self, device_id: &str) {
        self.0.lock().unwrap().remove(device_id);
    }

    fn get(&self, device_id: &str) -> Option<[u8; 32]> {
        self.0.lock().unwrap().get(device_id).copied()
    }
}

pub struct Approval {
    pub device_id: String,
    /// `channel.url` of the result (v1); `None` answers like a v0 Launchpad
    /// with an `ssh` object, from which the app derives the channel.
    pub channel_url: Option<String>,
    /// Pressing Cancel on the page: the callback gets `error=cancelled`.
    pub decline: bool,
    /// Sent as `environment.name` when set.
    pub environment_name: Option<String>,
    /// Where approved keys go.
    pub registry: Option<Arc<Registry>>,
}

pub struct FakeLaunchpad {
    pub origin: String,
    pub requests: Arc<Mutex<Vec<Query>>>,
    stop: Arc<AtomicBool>,
}

impl FakeLaunchpad {
    pub fn start(approval: Approval) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (reqs, stopped) = (requests.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &approval, &reqs),
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Self {
            origin,
            requests,
            stop,
        }
    }
}

impl Drop for FakeLaunchpad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve(mut stream: TcpStream, approval: &Approval, reqs: &Mutex<Vec<Query>>) {
    let _ = stream.set_nonblocking(false);
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    let target = head.split(' ').nth(1).unwrap_or("").to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    if path != "/connect/computer" {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let q: Query = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    reqs.lock().unwrap().push(q.clone());
    let callback = q.get("callback").cloned().unwrap_or_default();
    let state = q.get("state").cloned().unwrap_or_default();
    let mut location = url::Url::parse(&callback).expect("callback");
    if approval.decline {
        location
            .query_pairs_mut()
            .append_pair("state", &state)
            .append_pair("error", "cancelled");
    } else {
        if let Some(registry) = &approval.registry {
            registry.register(&approval.device_id, &q["pubkey"]);
        }
        let mut result = json!({ "deviceId": approval.device_id });
        match &approval.channel_url {
            Some(url) => result["channel"] = json!({ "url": url }),
            None => {
                result["ssh"] = json!({"host": "env.example.com", "user": "env", "knownHosts": "ssh-ed25519 AAAA"})
            }
        }
        if let Some(name) = &approval.environment_name {
            result["environment"] = json!({ "name": name });
        }
        location
            .query_pairs_mut()
            .append_pair("state", &state)
            .append_pair("result", &URL_SAFE_NO_PAD.encode(result.to_string()));
    }
    let _ = write!(
        stream,
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
}

/// What the system browser does with the approval URL: follow the redirect
/// and return the status and body of the app's callback page.
pub fn browse(url: &str) -> (u16, String) {
    match ureq::get(url).call() {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => (0, e.to_string()),
    }
}

/// Polls `check` until it returns true or `timeout` passes.
pub fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// How the fake Environment behaves.
#[derive(Clone)]
pub struct EnvOptions {
    /// The Environment id in the challenge.
    pub environment: String,
    /// How long a session stays open after the agent's script.
    pub hold: Duration,
    /// Wait before the agent's first tool call.
    pub call_delay: Duration,
    /// Shifts the challenge's `ts` (negative: an old challenge).
    pub ts_offset_ms: i64,
    /// Sends this nonce every time instead of a fresh one.
    pub fixed_nonce: Option<String>,
    /// Refuses every computer with this reason.
    pub deny: Option<String>,
    /// After `ready`, stops reading the socket: no pongs, no MCP.
    pub silent_after_ready: bool,
    /// Runs this through `lazurio_shell_run` when the computer lists it.
    pub shell_command: Option<String>,
}

impl Default for EnvOptions {
    fn default() -> Self {
        Self {
            environment: "test-environment".into(),
            hold: Duration::from_millis(200),
            call_delay: Duration::ZERO,
            ts_offset_ms: 0,
            fixed_nonce: None,
            deny: None,
            silent_after_ready: false,
            shell_command: None,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct EnvStats {
    pub connections: usize,
    pub sessions: usize,
    pub pings: usize,
    pub denied: Vec<String>,
}

pub struct FakeEnvironment {
    pub origin: String,
    pub channel_url: String,
    pub out: PathBuf,
    pub registry: Arc<Registry>,
    pub stats: Arc<Mutex<EnvStats>>,
    stop: Arc<AtomicBool>,
}

struct EnvState {
    options: EnvOptions,
    registry: Arc<Registry>,
    stats: Arc<Mutex<EnvStats>>,
    used_nonces: Mutex<HashSet<String>>,
    active: Mutex<HashSet<String>>,
    out: PathBuf,
}

impl FakeEnvironment {
    pub fn start(options: EnvOptions, out: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let registry = Arc::new(Registry::default());
        let stats = Arc::new(Mutex::new(EnvStats::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(EnvState {
            options,
            registry: registry.clone(),
            stats: stats.clone(),
            used_nonces: Mutex::new(HashSet::new()),
            active: Mutex::new(HashSet::new()),
            out: out.clone(),
        });
        let stopped = stop.clone();
        std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        std::thread::spawn(move || session(stream, &state));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Self {
            origin: format!("http://127.0.0.1:{port}"),
            channel_url: format!("ws://127.0.0.1:{port}/.lazurio/computers/attach"),
            out,
            registry,
            stats,
            stop,
        }
    }

    pub fn stats(&self) -> EnvStats {
        self.stats.lock().unwrap().clone()
    }
}

impl Drop for FakeEnvironment {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn deny(ws: &mut WebSocket<TcpStream>, state: &EnvState, reason: &str) {
    state.stats.lock().unwrap().denied.push(reason.to_string());
    let _ = ws.send(Message::text(
        json!({"type": "denied", "reason": reason}).to_string(),
    ));
    let _ = ws.close(None);
    let _ = ws.flush();
}

fn read_json(ws: &mut WebSocket<TcpStream>, deadline: Instant) -> Option<Value> {
    while Instant::now() < deadline {
        match ws.read() {
            Ok(Message::Text(t)) => return serde_json::from_str(t.as_str()).ok(),
            Ok(Message::Close(_)) => return None,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return None,
        }
    }
    None
}

#[allow(clippy::result_large_err)] // tungstenite's handshake callback type
fn session(stream: TcpStream, state: &EnvState) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut device = String::new();
    let mut ws = match tungstenite::accept_hdr(
        stream,
        |req: &tungstenite::handshake::server::Request, resp| {
            if req.uri().path() != "/.lazurio/computers/attach" {
                let mut r = tungstenite::handshake::server::ErrorResponse::new(None);
                *r.status_mut() = tungstenite::http::StatusCode::NOT_FOUND;
                return Err(r);
            }
            let query = req.uri().query().unwrap_or("");
            device = url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == "device")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            Ok(resp)
        },
    ) {
        Ok(ws) => ws,
        Err(_) => return,
    };
    state.stats.lock().unwrap().connections += 1;
    let opts = &state.options;
    let nonce = opts.fixed_nonce.clone().unwrap_or_else(|| {
        let mut raw = [0u8; 32];
        rand_bytes(&mut raw);
        URL_SAFE_NO_PAD.encode(raw)
    });
    let ts = now_ms() + opts.ts_offset_ms;
    let challenge = json!({"type": "challenge", "v": 1, "environment": opts.environment, "nonce": nonce, "ts": ts});
    if ws.send(Message::text(challenge.to_string())).is_err() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let Some(auth) = read_json(&mut ws, deadline) else {
        return;
    };
    if let Some(reason) = &opts.deny {
        return deny(&mut ws, state, reason);
    }
    let claimed = auth["device"].as_str().unwrap_or("");
    let Some(key) = state.registry.get(claimed).filter(|_| claimed == device) else {
        return deny(&mut ws, state, "unknown-device");
    };
    let message = format!(
        "lazurio-computer-attach-v1\n{}\n{}\n{}",
        opts.environment, nonce, ts
    );
    let verified = URL_SAFE_NO_PAD
        .decode(auth["signature"].as_str().unwrap_or(""))
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .is_some_and(|sig| {
            VerifyingKey::from_bytes(&key).is_ok_and(|k| k.verify(message.as_bytes(), &sig).is_ok())
        });
    if !verified {
        return deny(&mut ws, state, "bad-signature");
    }
    if now_ms() - ts > 60_000 {
        return deny(&mut ws, state, "stale-challenge");
    }
    if !state.used_nonces.lock().unwrap().insert(nonce.clone()) {
        return deny(&mut ws, state, "replayed-nonce");
    }
    if !state.active.lock().unwrap().insert(device.clone()) {
        return deny(&mut ws, state, "already-attached");
    }
    state.stats.lock().unwrap().sessions += 1;
    if ws.send(Message::text(r#"{"type":"ready"}"#)).is_ok() {
        if opts.silent_after_ready {
            std::thread::sleep(Duration::from_secs(5));
        } else {
            play_agent(&mut ws, state);
        }
    }
    let _ = ws.close(None);
    let _ = ws.flush();
    state.active.lock().unwrap().remove(&device);
}

fn rand_bytes(buf: &mut [u8]) {
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(buf);
}

/// The agent's side of one session, like the Environment's broker would do.
fn play_agent(ws: &mut WebSocket<TcpStream>, state: &EnvState) {
    let pings = state.stats.clone();
    let call = |ws: &mut WebSocket<TcpStream>, msg: Value| -> Value {
        let id = msg["id"].clone();
        if ws.send(Message::text(msg.to_string())).is_err() {
            return Value::Null;
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match ws.read() {
                Ok(Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                        if v["id"] == id {
                            return v;
                        }
                    }
                }
                Ok(Message::Ping(_)) => pings.lock().unwrap().pings += 1,
                Ok(Message::Close(_)) => return Value::Null,
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return Value::Null,
            }
        }
        Value::Null
    };
    let init = call(
        ws,
        json!({"jsonrpc": "2.0", "id": "b0", "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "fake-environment", "version": "0"}}}),
    );
    let _ = ws.send(Message::text(
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
    ));
    let ping = call(ws, json!({"jsonrpc": "2.0", "id": "p1", "method": "ping"}));
    let list = call(
        ws,
        json!({"jsonrpc": "2.0", "id": "l1", "method": "tools/list", "params": {}}),
    );
    let names: Vec<String> = list["result"]["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let app_status = if names.iter().any(|n| n == "lazurio_app_status") {
        call(
            ws,
            json!({"jsonrpc": "2.0", "id": "s1", "method": "tools/call", "params": {
            "name": "lazurio_app_status", "arguments": {}}}),
        )["result"]["content"][0]["text"]
            .clone()
    } else {
        Value::Null
    };
    if !idle(ws, state.options.call_delay, &pings) {
        return;
    }
    let c1 = call(
        ws,
        json!({"jsonrpc": "2.0", "id": "c1", "method": "tools/call", "params": {
        "name": "desktop_fake_tool", "arguments": {"app": "Fake App", "text": "do-not-log-this"}}}),
    );
    let c2 = call(
        ws,
        json!({"jsonrpc": "2.0", "id": "c2", "method": "tools/call", "params": {
        "name": "desktop_fail", "arguments": {}}}),
    );
    let shell = match &state.options.shell_command {
        Some(command) if names.iter().any(|n| n == "lazurio_shell_run") => {
            let r = call(
                ws,
                json!({"jsonrpc": "2.0", "id": "x1", "method": "tools/call", "params": {
                "name": "lazurio_shell_run", "arguments": {"command": command, "timeout_s": 30}}}),
            );
            r["result"]["content"][0]["text"]
                .as_str()
                .and_then(|t| serde_json::from_str::<Value>(t).ok())
                .unwrap_or(Value::Null)
        }
        _ => Value::Null,
    };
    let summary = json!({
        "shell": shell,
        "server": init["result"]["serverInfo"]["name"],
        "app_meta": init["result"]["_meta"]["io.lazurio/agent-desktop"],
        "ping": ping["result"].is_object(),
        "tools": names.len(),
        "app_status": app_status,
        "c1_ok": c1["result"]["isError"] == false,
        "c1_text": c1["result"]["content"][0]["text"],
        "c2_error": c2["result"]["isError"] == true,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.out)
    {
        let _ = f.write_all(format!("{summary}\n").as_bytes());
    }
    // Stay connected for a while, answering the computer's pings.
    idle(ws, state.options.hold, &pings);
}

/// Keeps reading (so the computer's pings get their pongs) for `duration`.
/// Returns false when the computer closed the session.
fn idle(ws: &mut WebSocket<TcpStream>, duration: Duration, stats: &Mutex<EnvStats>) -> bool {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        match ws.read() {
            Ok(Message::Ping(_)) => stats.lock().unwrap().pings += 1,
            Ok(Message::Close(_)) => return false,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return false,
        }
    }
    true
}
