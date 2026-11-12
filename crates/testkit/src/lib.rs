//! A fake Launchpad for pairing tests.
//!
//! `GET /connect/computer?…` acts as if the owner had signed in and pressed
//! "Allow agents on this computer": it calls the `on_approve` hook (which can
//! write `authorized_keys` like the real Launchpad does) and redirects the
//! browser to the app's callback with the contract's `result`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::json;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Query = HashMap<String, String>;
pub type Hook = Box<dyn Fn(&Query, &str) + Send + Sync>;

pub struct Approval {
    pub device_id: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    /// Sent as Launchpad would: a key line, with or without a host name.
    pub known_hosts: String,
    /// Pressing Cancel on the page: the callback gets `error=cancelled`.
    pub decline: bool,
    /// Sent as `environment.name` when set (proposed addition to contract v0).
    pub environment_name: Option<String>,
    /// Called with the query and the device id before redirecting.
    pub on_approve: Option<Hook>,
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
        if let Some(hook) = &approval.on_approve {
            hook(&q, &approval.device_id);
        }
        let mut result = json!({
            "deviceId": approval.device_id,
            "ssh": {
                "host": approval.host,
                "user": approval.user,
                "port": approval.port,
                "knownHosts": approval.known_hosts,
            }
        });
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
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check()
}
