//! The channel (contract v0, section 2).
//!
//! While the owner's switch is on, the supervisor keeps one session alive: it
//! starts the system OpenSSH client towards the Environment and the Agent
//! Desktop MCP server (`agent-desktop mcp`), and joins the stdio of the two
//! line by line. The Environment's forced command speaks MCP to the computer,
//! so the computer is the MCP server and nothing on it listens on the network.
//!
//! When the session ends and the switch is still on, the supervisor connects
//! again after a growing delay. Turning the switch off or [`Supervisor::stop_now`]
//! ends the session at once.

use crate::app_tools::{AppToolsHandle, Route, Router};
use crate::history::{Event, History, HistoryEntry};
use crate::mcp_tap::{Tap, TapEvent};
use crate::pairing::SshTarget;
use crate::platform::{hide_console, write_atomic, write_private_file};
use crate::time::{now_ms, rfc3339};
use crate::Result;
use rand_core::{OsRng, RngCore};
use serde::Serialize;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// How the engine is started; the app points it at the bundled sidecar.
#[derive(Clone, Debug)]
pub struct EngineSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct ChannelConfig {
    pub env_id: String,
    pub ssh_program: PathBuf,
    pub target: SshTarget,
    pub known_hosts_file: PathBuf,
    /// Short-lived files such as the key handed to `ssh`.
    pub run_dir: PathBuf,
    pub engine: EngineSpec,
    /// Extra `-o` options, for tests only.
    pub extra_ssh_options: Vec<String>,
    /// Tools the app serves itself next to the engine's.
    pub app_tools: Option<AppToolsHandle>,
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
    HostKey,
    NotAuthorized,
    AlreadyConnected,
    SshMissing,
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
            Reason::HostKey => "host-key",
            Reason::NotAuthorized => "not-authorized",
            Reason::AlreadyConnected => "already-connected",
            Reason::SshMissing => "ssh-missing",
            Reason::EngineMissing => "engine-missing",
            Reason::KeyMissing => "key-missing",
            Reason::Stopped => "stopped",
            Reason::SwitchedOff => "switched-off",
            Reason::Ended => "ended",
            Reason::Error => "error",
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

pub type KeyProvider = Arc<dyn Fn() -> Result<Zeroizing<String>> + Send + Sync>;
pub type StatusListener = Arc<dyn Fn(&str, &ChannelStatus) + Send + Sync>;

/// Seconds a working state lasts after the last tool call finished.
const WORKING_LINGER: Duration = Duration::from_secs(20);
/// The key file is deleted as soon as the Environment speaks; at the latest after this.
const KEY_FILE_MAX_AGE: Duration = Duration::from_secs(30);
/// A session that ran this long resets the reconnect delay.
const STABLE_SESSION: Duration = Duration::from_secs(30);
const RECONNECT_AFTER_STOP: Duration = Duration::from_secs(3);

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
    EnvironmentSpoke,
    Tap(TapEvent),
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
        if !self.cfg.ssh_program.exists() {
            return fail(Reason::SshMissing);
        }
        if !self.cfg.engine.program.exists() {
            return fail(Reason::EngineMissing);
        }
        let pem = match (self.key)() {
            Ok(pem) => pem,
            Err(_) => return fail(Reason::KeyMissing),
        };
        let known_hosts = self.cfg.target.known_hosts.join("\n") + "\n";
        if write_atomic(&self.cfg.known_hosts_file, known_hosts.as_bytes()).is_err() {
            return fail(Reason::Error);
        }
        let _ = std::fs::create_dir_all(&self.cfg.run_dir);
        let key_file = KeyFile::create(&self.cfg.run_dir, &self.cfg.env_id, pem.as_bytes());
        drop(pem);
        let key_file = match key_file {
            Ok(k) => k,
            Err(_) => return fail(Reason::KeyMissing),
        };

        let mut engine_cmd = Command::new(&self.cfg.engine.program);
        engine_cmd
            .args(&self.cfg.engine.args)
            .envs(self.cfg.engine.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut engine = match hide_console(&mut engine_cmd).spawn() {
            Ok(c) => Kill(c),
            Err(_) => return fail(Reason::EngineMissing),
        };
        let mut ssh_cmd = Command::new(&self.cfg.ssh_program);
        ssh_cmd
            .args(ssh_args(
                &self.cfg.target,
                &self.cfg.known_hosts_file,
                key_file.path(),
                &self.cfg.extra_ssh_options,
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut ssh = match hide_console(&mut ssh_cmd).spawn() {
            Ok(c) => Kill(c),
            Err(_) => return fail(Reason::SshMissing),
        };

        let (tx, rx) = mpsc::channel::<Msg>();
        let tap = Arc::new(Mutex::new(Tap::default()));
        let router = Arc::new(Router::new(
            self.cfg.app_tools.as_ref().map(|h| h.0.clone()),
        ));
        // Engine answers and the app's own answers share the way to the Environment.
        let to_environment = Arc::new(Mutex::new(ssh.0.stdin.take().expect("piped")));
        let mut threads = Vec::new();
        {
            let from = ssh.0.stdout.take().expect("piped");
            let mut to = engine.0.stdin.take().expect("piped");
            let tap = tap.clone();
            let tx = tx.clone();
            let router = router.clone();
            let to_environment = to_environment.clone();
            threads.push(std::thread::spawn(move || {
                let mut first = true;
                for_each_line(from, |line| {
                    if first {
                        first = false;
                        let _ = tx.send(Msg::EnvironmentSpoke);
                    }
                    match router.route_environment_line(line) {
                        Route::Engine => {
                            for e in tap.lock().unwrap().from_environment(line) {
                                let _ = tx.send(Msg::Tap(e));
                            }
                            to.write_all(line).and_then(|_| to.flush()).is_ok()
                        }
                        Route::Local {
                            id,
                            name,
                            arguments,
                        } => {
                            let Some(tools) = router.tools().cloned() else {
                                return true;
                            };
                            let tx = tx.clone();
                            let to_environment = to_environment.clone();
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
                                let answer =
                                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                                let mut bytes = serde_json::to_vec(&answer).unwrap_or_default();
                                bytes.push(b'\n');
                                let mut out = to_environment.lock().unwrap();
                                let _ = out.write_all(&bytes).and_then(|_| out.flush());
                                drop(out);
                                let _ = tx.send(Msg::Tap(TapEvent::ToolFinished {
                                    tool: name,
                                    ok,
                                    ms: started.elapsed().as_millis() as u64,
                                }));
                            });
                            true
                        }
                    }
                });
            }));
        }
        {
            let from = engine.0.stdout.take().expect("piped");
            let tap = tap.clone();
            let tx = tx.clone();
            let router = router.clone();
            let to_environment = to_environment.clone();
            threads.push(std::thread::spawn(move || {
                for_each_line(from, |line| {
                    for e in tap.lock().unwrap().from_engine(line) {
                        let _ = tx.send(Msg::Tap(e));
                    }
                    let line = router.rewrite_engine_line(line);
                    let mut out = to_environment.lock().unwrap();
                    out.write_all(&line).and_then(|_| out.flush()).is_ok()
                });
            }));
        }
        drop(tx);
        let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::new()));
        {
            let mut err = ssh.0.stderr.take().expect("piped");
            let tail = stderr_tail.clone();
            threads.push(std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = err.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.lock().unwrap();
                    t.extend_from_slice(&buf[..n]);
                    let len = t.len();
                    if len > 8192 {
                        t.drain(..len - 8192);
                    }
                }
            }));
        }

        let mut key_file = Some(key_file);
        let mut initialized = false;
        let mut connected_at = Instant::now();
        let mut streak_since_ms = 0u64;
        let mut last_activity: Option<Instant> = None;
        let mut current_tool: Option<(String, Option<String>)> = None;
        let mut ended_by: Option<Reason> = None;

        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Msg::EnvironmentSpoke) => key_file = None,
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
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
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
            if key_file
                .as_ref()
                .is_some_and(|k| k.created.elapsed() >= KEY_FILE_MAX_AGE)
            {
                key_file = None;
            }
            if let Some(reason) = self.end_requested() {
                ended_by = Some(reason);
                break;
            }
            if matches!(ssh.0.try_wait(), Ok(Some(_))) || matches!(engine.0.try_wait(), Ok(Some(_)))
            {
                // Let the pumps hand over what is still in flight.
                std::thread::sleep(Duration::from_millis(200));
                break;
            }
        }

        drop(key_file);
        let ssh_status = ssh.finish();
        engine.finish();
        for t in threads {
            let _ = t.join();
        }
        let stderr = String::from_utf8_lossy(&stderr_tail.lock().unwrap()).to_string();
        let reason = ended_by.unwrap_or_else(|| classify(&stderr, initialized, ssh_status));
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

/// Kills the child when dropped, so no path leaves `ssh` or the engine running.
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

/// The private key as a file only the user can read, deleted on drop.
struct KeyFile {
    path: PathBuf,
    created: Instant,
}

impl KeyFile {
    fn create(dir: &Path, env_id: &str, pem: &[u8]) -> std::io::Result<Self> {
        let mut raw = [0u8; 8];
        OsRng.fill_bytes(&mut raw);
        let suffix: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("{env_id}-{suffix}.key"));
        write_private_file(&path, pem)?;
        Ok(Self {
            path,
            created: Instant::now(),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The OpenSSH command line from the contract, plus options that keep the
/// session independent of any user configuration.
pub fn ssh_args(
    target: &SshTarget,
    known_hosts_file: &Path,
    key_file: &Path,
    extra: &[String],
) -> Vec<OsString> {
    let kh = ssh_path_value(known_hosts_file);
    let mut args: Vec<OsString> = vec!["-F".into(), "none".into(), "-T".into()];
    args.push("-i".into());
    args.push(ssh_escape_tokens(&key_file.to_string_lossy()).into());
    for opt in [
        "IdentitiesOnly=yes".to_string(),
        "IdentityAgent=none".to_string(),
        "BatchMode=yes".to_string(),
        "PreferredAuthentications=publickey".to_string(),
        format!("UserKnownHostsFile={kh}"),
        format!("GlobalKnownHostsFile={kh}"),
        "StrictHostKeyChecking=yes".to_string(),
        "UpdateHostKeys=no".to_string(),
        "ClearAllForwardings=yes".to_string(),
        "ForwardAgent=no".to_string(),
        "ForwardX11=no".to_string(),
        "ServerAliveInterval=15".to_string(),
        "ServerAliveCountMax=2".to_string(),
        "ConnectTimeout=20".to_string(),
        "LogLevel=ERROR".to_string(),
    ]
    .into_iter()
    .chain(extra.iter().cloned())
    {
        args.push("-o".into());
        args.push(opt.into());
    }
    args.push("-p".into());
    args.push(target.port.to_string().into());
    args.push("-l".into());
    args.push(target.user.clone().into());
    args.push("--".into());
    args.push(target.host.clone().into());
    args
}

/// `-o Key=value` values are split on whitespace and expand `%` tokens, and
/// app data paths contain spaces ("Application Support"), so quote them.
fn ssh_path_value(path: &Path) -> String {
    let mut s = path.to_string_lossy().to_string();
    if cfg!(windows) {
        s = s.replace('\\', "/");
    }
    format!("\"{}\"", ssh_escape_tokens(&s).replace('"', "\\\""))
}

fn ssh_escape_tokens(s: &str) -> String {
    s.replace('%', "%%")
}

fn classify(stderr: &str, initialized: bool, exit: Option<i32>) -> Reason {
    let s = stderr.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| s.contains(n));
    if has(&[
        "host key verification failed",
        "remote host identification has changed",
        "no matching host key",
    ]) {
        Reason::HostKey
    } else if has(&["permission denied"]) {
        Reason::NotAuthorized
    } else if has(&[
        "already attached",
        "already connected",
        "už připojen",
        "jinou relací",
    ]) {
        Reason::AlreadyConnected
    } else if has(&[
        "could not resolve hostname",
        "name or service not known",
        "nodename nor servname",
        "no such host",
        "no route to host",
        "network is unreachable",
        "timed out",
        "connection refused",
        "connection reset",
        "connection closed",
        "broken pipe",
        "not responding",
    ]) {
        Reason::Offline
    } else if initialized || exit == Some(0) {
        Reason::Ended
    } else {
        Reason::Error
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

    fn target() -> SshTarget {
        SshTarget {
            host: "env.tailnet.example.com".into(),
            user: "env".into(),
            port: 22,
            known_hosts: vec![],
        }
    }

    #[test]
    fn command_line_follows_the_contract() {
        let args: Vec<String> = ssh_args(
            &target(),
            Path::new(
                "/Users/x/Library/Application Support/io.lazurio.agent-desktop/known_hosts/a",
            ),
            Path::new("/tmp/k%1.key"),
            &[],
        )
        .into_iter()
        .map(|a| a.into_string().unwrap())
        .collect();
        let joined = args.join(" ");
        assert!(joined.starts_with("-F none -T -i /tmp/k%%1.key"));
        for opt in [
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "ClearAllForwardings=yes",
            "ForwardAgent=no",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=2",
        ] {
            assert!(args.iter().any(|a| a == opt), "{opt}");
        }
        assert!(args.contains(
            &"UserKnownHostsFile=\"/Users/x/Library/Application Support/io.lazurio.agent-desktop/known_hosts/a\""
                .to_string()
        ));
        assert_eq!(
            &args[args.len() - 6..],
            ["-p", "22", "-l", "env", "--", "env.tailnet.example.com"]
        );
    }

    #[test]
    fn classifies_ssh_errors() {
        assert_eq!(
            classify("Host key verification failed.\r\n", false, Some(255)),
            Reason::HostKey
        );
        assert_eq!(
            classify("x@y: Permission denied (publickey).", false, Some(255)),
            Reason::NotAuthorized
        );
        assert_eq!(
            classify(
                "ssh: Could not resolve hostname h: nodename nor servname provided",
                false,
                Some(255)
            ),
            Reason::Offline
        );
        assert_eq!(
            classify(
                "Počítač x je už připojený jinou relací. (Computer x is already attached in another session.)",
                false,
                Some(2)
            ),
            Reason::AlreadyConnected
        );
        assert_eq!(classify("", true, Some(0)), Reason::Ended);
        assert_eq!(classify("", false, Some(1)), Reason::Error);
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
