//! The channel (contract v1, section 2).
//!
//! While the owner's switch is on, the supervisor keeps one session alive: it
//! opens the WebSocket to the Environment's Launchpad, proves the computer's
//! identity by signing the Environment's challenge with the device key (in
//! memory), and after `ready` starts the Agent Desktop MCP server
//! (`agent-desktop mcp`) and joins it to the socket: one text frame is one MCP
//! message. The Environment speaks MCP to the computer, so the computer is the
//! MCP server and nothing on it listens on the network.
//!
//! When the session ends and the switch is still on, the supervisor connects
//! again after a growing delay. Turning the switch off or [`Supervisor::stop_now`]
//! ends the session at once.

use crate::app_tools::{AppToolsHandle, LockSeat, Route, Router};
use crate::history::{Event, History, HistoryEntry};
use crate::lock::{busy_message, ComputerLock};
use crate::mcp_tap::{Tap, TapEvent};
use crate::platform::hide_console;
use crate::process_tree::ProcessTree;
use crate::time::{now_ms, rfc3339};
use crate::wire::{self, Socket, WireError};
use crate::Result;
use rand_core::{OsRng, RngCore};
use serde::Serialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tungstenite::Message;
use zeroize::Zeroizing;

/// How the engine is started; the app points it at the bundled sidecar.
#[derive(Clone, Debug)]
pub struct EngineSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Keep-alive and timeouts; tests shorten them.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub connect_timeout: Duration,
    pub ping_every: Duration,
    pub dead_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(20),
            ping_every: Duration::from_secs(15),
            dead_after: Duration::from_secs(45),
        }
    }
}

#[derive(Clone)]
pub struct ChannelConfig {
    pub env_id: String,
    /// The Environment's name, for the computer-wide lock's messages.
    pub env_label: String,
    /// `wss://<launchpad host>/.lazurio/computers/attach`.
    pub channel_url: String,
    pub device_id: String,
    pub engine: EngineSpec,
    /// Tools the app serves itself next to the engine's.
    pub app_tools: Option<AppToolsHandle>,
    /// One agent at a time across all Environments.
    pub lock: Arc<ComputerLock>,
    pub timing: Timing,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Off,
    Connecting,
    Connected,
    Working,
    Waiting,
}

/// Why a session ended or could not start. Fixed vocabulary; the history and
/// the panel use [`Reason::code`].
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    Offline,
    /// The Environment's certificate did not verify.
    Certificate,
    /// The Environment answered the upgrade with 404: no HTTPS channel yet.
    Unsupported,
    NotAuthorized,
    AlreadyConnected,
    EngineMissing,
    KeyMissing,
    Stopped,
    SwitchedOff,
    Ended,
    Error,
}

impl Reason {
    pub fn code(self) -> &'static str {
        match self {
            Reason::Offline => "offline",
            Reason::Certificate => "certificate",
            Reason::Unsupported => "unsupported",
            Reason::NotAuthorized => "not-authorized",
            Reason::AlreadyConnected => "already-connected",
            Reason::EngineMissing => "engine-missing",
            Reason::KeyMissing => "key-missing",
            Reason::Stopped => "stopped",
            Reason::SwitchedOff => "switched-off",
            Reason::Ended => "ended",
            Reason::Error => "error",
        }
    }

    fn from_wire(e: &WireError) -> Self {
        match e {
            WireError::Offline(_) => Reason::Offline,
            WireError::Certificate(_) => Reason::Certificate,
            WireError::Http(404) => Reason::Unsupported,
            WireError::Http(401 | 403) => Reason::NotAuthorized,
            WireError::Http(_) => Reason::Offline,
            WireError::Denied(r) => match r.as_str() {
                "already-attached" | "already-connected" => Reason::AlreadyConnected,
                "unknown-device" | "not-paired" | "bad-signature" | "removed" => {
                    Reason::NotAuthorized
                }
                _ => Reason::Error,
            },
            WireError::Protocol(_) => Reason::Error,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelStatus {
    pub state: State,
    /// Unix milliseconds since the current state (or working streak) began.
    pub since_ms: u64,
    pub tool: Option<String>,
    /// Shown live while a tool runs; never written to the history.
    pub app: Option<String>,
    pub retry_at_ms: Option<u64>,
    pub reason: Option<Reason>,
}

impl ChannelStatus {
    fn new(state: State) -> Self {
        Self {
            state,
            since_ms: now_ms(),
            tool: None,
            app: None,
            retry_at_ms: None,
            reason: None,
        }
    }
}

/// The device key's 32-byte seed, from the system key store, kept in memory only.
pub type KeyProvider = Arc<dyn Fn() -> Result<Zeroizing<[u8; 32]>> + Send + Sync>;
pub type StatusListener = Arc<dyn Fn(&str, &ChannelStatus) + Send + Sync>;

/// Seconds a working state lasts after the last tool call finished.
const WORKING_LINGER: Duration = Duration::from_secs(20);
/// A session that ran this long resets the reconnect delay.
const STABLE_SESSION: Duration = Duration::from_secs(30);
const RECONNECT_AFTER_STOP: Duration = Duration::from_secs(3);
/// How long ending a session waits for its processes to be gone.
const TREE_END_TIMEOUT: Duration = Duration::from_secs(5);

struct Ctl {
    enabled: bool,
    kill: bool,
    kick: bool,
    shutdown: bool,
    status: ChannelStatus,
}

struct Shared {
    ctl: Mutex<Ctl>,
    cv: Condvar,
}

pub struct Supervisor {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl Supervisor {
    pub fn spawn(
        cfg: ChannelConfig,
        key: KeyProvider,
        history: Arc<History>,
        listener: StatusListener,
        enabled: bool,
    ) -> Self {
        let shared = Arc::new(Shared {
            ctl: Mutex::new(Ctl {
                enabled,
                kill: false,
                kick: false,
                shutdown: false,
                status: ChannelStatus::new(State::Off),
            }),
            cv: Condvar::new(),
        });
        let worker = Worker {
            cfg,
            key,
            history,
            listener,
            shared: shared.clone(),
        };
        let handle = std::thread::Builder::new()
            .name("lazurio-channel".into())
            .spawn(move || worker.run())
            .expect("spawn channel thread");
        Self {
            shared,
            handle: Some(handle),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        let mut c = self.shared.ctl.lock().unwrap();
        if c.enabled != enabled {
            c.enabled = enabled;
            c.kick = enabled;
            self.shared.cv.notify_all();
        }
    }

    /// Ends the current session immediately. The switch stays on, so the
    /// channel comes back after a short pause.
    pub fn stop_now(&self) {
        let mut c = self.shared.ctl.lock().unwrap();
        c.kill = true;
        self.shared.cv.notify_all();
    }

    pub fn status(&self) -> ChannelStatus {
        self.shared.ctl.lock().unwrap().status.clone()
    }

    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        {
            let mut c = self.shared.ctl.lock().unwrap();
            c.shutdown = true;
            self.shared.cv.notify_all();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

struct Worker {
    cfg: ChannelConfig,
    key: KeyProvider,
    history: Arc<History>,
    listener: StatusListener,
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Outcome {
    initialized: bool,
    lasted: Duration,
    reason: Reason,
}

enum Msg {
    Tap(TapEvent),
    /// One MCP message for the Environment.
    Out(String),
}

impl Worker {
    fn set_status(&self, status: ChannelStatus) {
        let changed = {
            let mut c = self.shared.ctl.lock().unwrap();
            let changed = c.status.state != status.state
                || c.status.tool != status.tool
                || c.status.app != status.app
                || c.status.reason != status.reason
                || c.status.retry_at_ms != status.retry_at_ms;
            if changed {
                c.status = status.clone();
            }
            changed
        };
        if changed {
            (self.listener)(&self.cfg.env_id, &status);
        }
    }

    fn run(self) {
        let mut backoff = Backoff::default();
        loop {
            {
                let mut c = self.shared.ctl.lock().unwrap();
                while !c.shutdown && !c.enabled {
                    if c.status.state != State::Off {
                        drop(c);
                        self.set_status(ChannelStatus::new(State::Off));
                        c = self.shared.ctl.lock().unwrap();
                        continue;
                    }
                    c = self.shared.cv.wait(c).unwrap();
                }
                if c.shutdown {
                    return;
                }
                c.kill = false;
                c.kick = false;
            }
            self.set_status(ChannelStatus::new(State::Connecting));
            let outcome = self.session();
            let delay = {
                let c = self.shared.ctl.lock().unwrap();
                if c.shutdown {
                    return;
                }
                if !c.enabled {
                    continue;
                }
                if outcome.reason == Reason::Stopped {
                    backoff.reset();
                    RECONNECT_AFTER_STOP
                } else {
                    if outcome.initialized && outcome.lasted >= STABLE_SESSION {
                        backoff.reset();
                    }
                    backoff.next()
                }
            };
            let retry_at = Instant::now() + delay;
            let mut waiting = ChannelStatus::new(State::Waiting);
            waiting.retry_at_ms = Some(now_ms() + delay.as_millis() as u64);
            waiting.reason = Some(outcome.reason);
            self.set_status(waiting);
            let mut c = self.shared.ctl.lock().unwrap();
            loop {
                if c.shutdown || !c.enabled {
                    break;
                }
                if c.kick {
                    backoff.reset();
                    break;
                }
                let now = Instant::now();
                if now >= retry_at {
                    break;
                }
                c = self.shared.cv.wait_timeout(c, retry_at - now).unwrap().0;
            }
        }
    }

    fn end_requested(&self) -> Option<Reason> {
        let c = self.shared.ctl.lock().unwrap();
        if c.shutdown || !c.enabled {
            Some(Reason::SwitchedOff)
        } else if c.kill {
            Some(Reason::Stopped)
        } else {
            None
        }
    }

    fn session(&self) -> Outcome {
        let started = Instant::now();
        let fail = |reason| Outcome {
            initialized: false,
            lasted: started.elapsed(),
            reason,
        };
        if !self.cfg.engine.program.exists() {
            return fail(Reason::EngineMissing);
        }
        let seed = match (self.key)() {
            Ok(seed) => seed,
            Err(_) => return fail(Reason::KeyMissing),
        };
        let timing = self.cfg.timing;
        let url = match wire::attach_url(&self.cfg.channel_url, &self.cfg.device_id) {
            Ok(url) => url,
            Err(_) => return fail(Reason::Error),
        };
        let mut socket = match wire::connect(&url, timing.connect_timeout) {
            Ok(s) => s,
            Err(e) => return fail(Reason::from_wire(&e)),
        };
        let auth = wire::authenticate(
            &mut socket,
            &self.cfg.device_id,
            &seed,
            timing.connect_timeout,
        );
        drop(seed);
        if let Err(e) = auth {
            return fail(Reason::from_wire(&e));
        }
        if self.end_requested().is_some() {
            let _ = socket.close(None);
            return fail(Reason::SwitchedOff);
        }

        // Only a ready channel gets an engine.
        let mut engine_cmd = Command::new(&self.cfg.engine.program);
        engine_cmd
            .args(&self.cfg.engine.args)
            .envs(self.cfg.engine.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut engine = match hide_console(&mut engine_cmd).spawn() {
            Ok(c) => Kill(c),
            Err(_) => {
                let _ = socket.close(None);
                return fail(Reason::EngineMissing);
            }
        };
        // The engine and everything it starts end together (Windows: one job).
        let tree = ProcessTree::new();
        tree.adopt(&engine.0);

        let (tx, rx) = mpsc::channel::<Msg>();
        let tap = Arc::new(Mutex::new(Tap::default()));
        let router = Arc::new(Router::with_lock(
            self.cfg.app_tools.as_ref().map(|h| h.0.clone()),
            LockSeat {
                lock: self.cfg.lock.clone(),
                env_id: self.cfg.env_id.clone(),
                label: self.cfg.env_label.clone(),
            },
        ));
        let mut threads = Vec::new();
        // To the engine: its own writer, so a slow engine never stalls the socket.
        let (to_engine, engine_inbox) = mpsc::channel::<Vec<u8>>();
        {
            let mut stdin = engine.0.stdin.take().expect("piped");
            threads.push(std::thread::spawn(move || {
                for line in engine_inbox {
                    if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
                        break;
                    }
                }
            }));
        }
        // From the engine: one line, one frame.
        {
            let stdout = engine.0.stdout.take().expect("piped");
            let tap = tap.clone();
            let tx = tx.clone();
            let router = router.clone();
            threads.push(std::thread::spawn(move || {
                for_each_line(stdout, |line| {
                    for e in tap.lock().unwrap().from_engine(line) {
                        let _ = tx.send(Msg::Tap(e));
                    }
                    let line = router.rewrite_engine_line(line);
                    let text = String::from_utf8_lossy(&line).trim_end().to_string();
                    tx.send(Msg::Out(text)).is_ok()
                });
            }));
        }

        let _ = socket.get_ref().set_read_timeout(Duration::from_millis(20));
        let mut initialized = false;
        let mut connected_at = Instant::now();
        let mut streak_since_ms = 0u64;
        let mut last_activity: Option<Instant> = None;
        let mut current_tool: Option<(String, Option<String>)> = None;
        let mut ended_by: Option<Reason> = None;
        let mut last_heard = Instant::now();
        let mut last_ping = Instant::now();

        'session: loop {
            // What the engine and the app's own tools want to say.
            loop {
                match rx.try_recv() {
                    Ok(Msg::Out(text)) => {
                        if socket.send(Message::text(text)).is_err() {
                            ended_by = Some(Reason::Offline);
                            break 'session;
                        }
                    }
                    Ok(Msg::Tap(TapEvent::Initialized)) => {
                        initialized = true;
                        connected_at = Instant::now();
                        self.history.record(&self.cfg.env_id, Event::Connected);
                        self.set_status(ChannelStatus::new(State::Connected));
                    }
                    Ok(Msg::Tap(TapEvent::ToolStarted { tool, app })) => {
                        if last_activity.is_none() {
                            streak_since_ms = now_ms();
                        }
                        last_activity = Some(Instant::now());
                        current_tool = Some((tool, app));
                    }
                    Ok(Msg::Tap(TapEvent::ToolFinished { tool, ok, ms })) => {
                        last_activity = Some(Instant::now());
                        self.finish_tool(&router, tool, ok, ms);
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }

            // What the Environment says.
            match socket.read() {
                Ok(Message::Text(text)) => {
                    last_heard = Instant::now();
                    let mut line = text.as_bytes().to_vec();
                    match router.route_environment_line(&line) {
                        Route::Engine => {
                            for e in tap.lock().unwrap().from_environment(&line) {
                                let _ = tx.send(Msg::Tap(e));
                            }
                            line.push(b'\n');
                            let _ = to_engine.send(line);
                        }
                        Route::Refuse { id, holder } => {
                            let answer = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": crate::app_tools::text_result(busy_message(&holder), true),
                            });
                            let _ = tx.send(Msg::Out(answer.to_string()));
                        }
                        Route::Local {
                            id,
                            name,
                            arguments,
                        } => {
                            if let Some(tools) = router.tools().cloned() {
                                let tx = tx.clone();
                                // Its own thread: an update check may take a while
                                // and the Environment's pings must keep flowing.
                                std::thread::spawn(move || {
                                    let started = Instant::now();
                                    let app = arguments["app"].as_str().map(str::to_string);
                                    let _ = tx.send(Msg::Tap(TapEvent::ToolStarted {
                                        tool: name.clone(),
                                        app,
                                    }));
                                    let result = tools.call(&name, &arguments);
                                    let ok = !result["isError"].as_bool().unwrap_or(false);
                                    let answer = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                                    let _ = tx.send(Msg::Out(answer.to_string()));
                                    let _ = tx.send(Msg::Tap(TapEvent::ToolFinished {
                                        tool: name,
                                        ok,
                                        ms: started.elapsed().as_millis() as u64,
                                    }));
                                });
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => break 'session,
                Ok(_) => last_heard = Instant::now(),
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    break 'session
                }
                Err(_) => {
                    ended_by = Some(Reason::Offline);
                    break 'session;
                }
            }

            // Keep-alive.
            if last_heard.elapsed() >= timing.dead_after {
                ended_by = Some(Reason::Offline);
                break;
            }
            if last_ping.elapsed() >= timing.ping_every {
                last_ping = Instant::now();
                if socket.send(Message::Ping(Vec::new().into())).is_err() {
                    ended_by = Some(Reason::Offline);
                    break;
                }
            }

            if initialized {
                let busy = tap.lock().unwrap().pending() > 0;
                let lingering = last_activity.is_some_and(|t| t.elapsed() < WORKING_LINGER);
                if busy || lingering {
                    let (tool, app) = current_tool.clone().unzip();
                    self.set_status(ChannelStatus {
                        state: State::Working,
                        since_ms: streak_since_ms,
                        tool,
                        app: app.flatten(),
                        retry_at_ms: None,
                        reason: None,
                    });
                } else if last_activity.take().is_some() {
                    current_tool = None;
                    self.set_status(ChannelStatus::new(State::Connected));
                }
            }
            if let Some(reason) = self.end_requested() {
                ended_by = Some(reason);
                break;
            }
            if matches!(engine.0.try_wait(), Ok(Some(_))) {
                ended_by = Some(Reason::Error);
                break;
            }
        }

        let _ = socket.close(None);
        let _ = flush_close(&mut socket);
        drop(to_engine);
        tree.end(TREE_END_TIMEOUT);
        engine.finish();
        for t in threads {
            let _ = t.join();
        }
        // Tool calls that finished just before the session ended still belong
        // in the history.
        while let Ok(msg) = rx.try_recv() {
            if let Msg::Tap(TapEvent::ToolFinished { tool, ok, ms }) = msg {
                self.finish_tool(&router, tool, ok, ms);
            }
        }
        self.cfg.lock.release(&self.cfg.env_id);
        let reason = ended_by.unwrap_or(Reason::Ended);
        if initialized {
            let mut entry = HistoryEntry::new(&self.cfg.env_id, Event::Disconnected);
            entry.reason = Some(reason.code().into());
            self.history.append(entry);
        }
        Outcome {
            initialized,
            lasted: if initialized {
                connected_at.elapsed()
            } else {
                Duration::ZERO
            },
            reason,
        }
    }

    fn finish_tool(&self, router: &Router, tool: String, ok: bool, ms: u64) {
        if router.holds_for(&tool) {
            self.cfg.lock.end_call(&self.cfg.env_id);
        }
        self.history.append(HistoryEntry {
            ts: rfc3339(now_ms()),
            env: self.cfg.env_id.clone(),
            event: Event::Tool,
            tool: Some(tool),
            ok: Some(ok),
            ms: Some(ms),
            reason: None,
        });
    }
}

/// Lets the close frame go out before the socket is dropped.
fn flush_close(socket: &mut Socket) -> tungstenite::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match socket.read() {
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Calls `handle` with every line (newline included) until EOF, an error, or
/// `handle` returns false.
fn for_each_line(from: impl Read, mut handle: impl FnMut(&[u8]) -> bool) {
    let mut reader = BufReader::with_capacity(64 * 1024, from);
    let mut line = Vec::with_capacity(64 * 1024);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if !handle(&line) {
                    break;
                }
            }
        }
    }
}

/// Kills the child when dropped, so no path leaves the engine running.
struct Kill(Child);

impl Kill {
    fn finish(&mut self) -> Option<i32> {
        if let Ok(Some(status)) = self.0.try_wait() {
            return status.code();
        }
        let _ = self.0.kill();
        self.0.wait().ok().and_then(|s| s.code())
    }
}

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Default)]
struct Backoff {
    attempt: u32,
}

impl Backoff {
    const MAX: Duration = Duration::from_secs(60);

    fn reset(&mut self) {
        self.attempt = 0;
    }

    fn next(&mut self) -> Duration {
        let base = Duration::from_secs(1u64 << self.attempt.min(6)).min(Self::MAX);
        self.attempt = self.attempt.saturating_add(1);
        // ±20 % so that many computers do not reconnect in lockstep.
        let jitter = (OsRng.next_u32() % 41) as f64 / 100.0 + 0.8;
        base.mul_f64(jitter).min(Self::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_wire_errors_to_reasons() {
        assert_eq!(
            Reason::from_wire(&WireError::Http(404)),
            Reason::Unsupported
        );
        assert_eq!(
            Reason::from_wire(&WireError::Denied("already-attached".into())),
            Reason::AlreadyConnected
        );
        assert_eq!(
            Reason::from_wire(&WireError::Denied("bad-signature".into())),
            Reason::NotAuthorized
        );
        assert_eq!(
            Reason::from_wire(&WireError::Certificate("x".into())),
            Reason::Certificate
        );
        assert_eq!(
            Reason::from_wire(&WireError::Offline("x".into())),
            Reason::Offline
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        let mut b = Backoff::default();
        let first = b.next();
        assert!(first >= Duration::from_millis(800) && first <= Duration::from_millis(1200));
        for _ in 0..10 {
            assert!(b.next() <= Backoff::MAX);
        }
        b.reset();
        assert!(b.next() <= Duration::from_millis(1200));
    }
}
