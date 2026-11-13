//! Commands on this computer (contract v1, section 3).
//!
//! With its own switch per Environment (off by default), an agent gets the
//! system's command line under the signed-in user's account: `zsh -lc` on
//! macOS, the user's login shell (`$SHELL -lc`) on Linux and PowerShell on
//! Windows. Commands run in the background without a terminal window and
//! without administrator rights: nothing may ask for an administrator
//! password or a UAC consent, and a command that needs them gets a plain
//! error instead. Every command runs in its own process tree, so Stop and the
//! switch end it with everything it started. The history keeps the command
//! and its exit code, never its output.

use crate::app_tools::text_result;
use crate::history::{Event, History, HistoryEntry};
use crate::lock::ComputerLock;
use crate::platform::hide_console;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TOOL_RUN: &str = "lazurio_shell_run";
pub const TOOL_START: &str = "lazurio_shell_start";
pub const TOOL_READ: &str = "lazurio_shell_read";
pub const TOOL_WRITE: &str = "lazurio_shell_write";
pub const TOOL_STOP: &str = "lazurio_shell_stop";
pub const TOOLS: &[&str] = &[TOOL_RUN, TOOL_START, TOOL_READ, TOOL_WRITE, TOOL_STOP];

/// Output kept per stream and returned per call.
const OUTPUT_LIMIT: usize = 64 * 1024;
/// What a background process may have waiting before the oldest output goes.
const BUFFER_LIMIT: usize = 1024 * 1024;
const RUN_TIMEOUT_DEFAULT: u64 = 120;
const RUN_TIMEOUT_MAX: u64 = 3600;
const READ_WAIT_MAX: u64 = 30;
/// At most this many background processes per Environment.
const MAX_PROCESSES: usize = 8;
/// The command as the history keeps it.
const HISTORY_COMMAND_LIMIT: usize = 500;

pub fn is_shell_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// MCP definitions of the five tools.
pub fn definitions() -> Vec<Value> {
    let shell = if cfg!(windows) {
        "PowerShell"
    } else if cfg!(target_os = "macos") {
        "zsh (login shell)"
    } else {
        "the user's login shell"
    };
    let common = format!(
        "Runs under the signed-in user's account with {shell}, in the background without a window and without administrator rights: commands that need sudo, an administrator password or a UAC consent fail with an error. Only one agent at a time may use this computer."
    );
    let command_props = json!({
        "command": {"type": "string", "description": "The command line, as typed into the shell."},
        "cwd": {"type": "string", "description": "Working folder (default: the user's home folder)."},
        "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Extra environment variables."}
    });
    let mut run_props = command_props.clone();
    run_props["timeout_s"] = json!({"type": "integer", "minimum": 1, "maximum": RUN_TIMEOUT_MAX, "description": "Seconds before the command is ended (default 120)."});
    vec![
        json!({
            "name": TOOL_RUN,
            "description": format!("Runs one command on this computer and waits for it: exit code, stdout and stderr (each cut to its last 64 KiB). {common}"),
            "inputSchema": {"type": "object", "properties": run_props, "required": ["command"], "additionalProperties": false}
        }),
        json!({
            "name": TOOL_START,
            "description": format!("Starts a long-running command on this computer (serial console, build monitor) and returns its id; use lazurio_shell_read, _write and _stop with it. At most 8 per Environment; the owner's Stop ends them. {common}"),
            "inputSchema": {"type": "object", "properties": command_props, "required": ["command"], "additionalProperties": false}
        }),
        json!({
            "name": TOOL_READ,
            "description": "New output of a started command since the last read, and whether it still runs (exit code when it ended). wait_s waits up to that many seconds for output.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"},
                "wait_s": {"type": "integer", "minimum": 0, "maximum": READ_WAIT_MAX}
            }, "required": ["id"], "additionalProperties": false}
        }),
        json!({
            "name": TOOL_WRITE,
            "description": "Writes text to the standard input of a started command (add \\n yourself for a line).",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"},
                "input": {"type": "string"}
            }, "required": ["id", "input"], "additionalProperties": false}
        }),
        json!({
            "name": TOOL_STOP,
            "description": "Ends a started command with everything it started.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false}
        }),
    ]
}

/// Why a command was not run. Plain words for the agent.
fn refusal(command: &str) -> Option<&'static str> {
    let lower = command.to_lowercase();
    if cfg!(windows) && (lower.contains("-verb runas") || lower.contains("-verb:runas")) {
        return Some("This command asks Windows for administrator rights (UAC). Lazurio Agent Desktop runs commands without administrator rights; ask the computer's owner to do this step.");
    }
    if cfg!(target_os = "macos") && lower.contains("with administrator privileges") {
        return Some("This command asks macOS for an administrator password. Lazurio Agent Desktop runs commands without administrator rights; ask the computer's owner to do this step.");
    }
    None
}

/// A hint appended when the output shows that administrator rights were needed.
fn admin_hint(stderr: &str) -> Option<&'static str> {
    let s = stderr.to_lowercase();
    let needs = s.contains("a password is required")
        || s.contains("a terminal is required")
        || s.contains("sudo: no tty")
        || s.contains("requires elevation")
        || s.contains("run as administrator")
        || s.contains("access is denied")
        || s.contains("permission denied");
    needs.then_some("The command seems to need administrator rights or a password. Lazurio Agent Desktop runs commands without them; ask the computer's owner to do this step.")
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("powershell.exe");
        let script = format!(
            "$OutputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8; {command}"
        );
        c.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ]);
        // Programs whose manifest asks for administrator rights run as the
        // user (and fail) instead of raising a UAC consent.
        c.env("__COMPAT_LAYER", "RunAsInvoker");
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("/bin/zsh");
        c.args(["-lc", command]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/bash".into());
        let mut c = Command::new(shell);
        c.args(["-lc", command]);
        c
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so ending it ends what it started.
        cmd.process_group(0);
        // sudo must never wait for a password.
        cmd.env("SUDO_ASKPASS", "/usr/bin/false");
    }
    hide_console(&mut cmd);
    cmd
}

/// A running command with its process tree.
struct Running {
    child: Child,
    #[cfg(windows)]
    tree: crate::process_tree::ProcessTree,
}

impl Running {
    fn spawn(command: &str, args: &Value) -> Result<(Self, Option<ChildStdin>, Captured), String> {
        let mut cmd = shell_command(command);
        let cwd = args["cwd"]
            .as_str()
            .map(std::path::PathBuf::from)
            .or_else(home_dir);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        if let Some(env) = args["env"].as_object() {
            for (k, v) in env {
                if let Some(v) = v.as_str() {
                    cmd.env(k, v);
                }
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("The command could not start: {e}"))?;
        #[cfg(windows)]
        let tree = {
            let tree = crate::process_tree::ProcessTree::new();
            tree.adopt(&child);
            tree
        };
        let stdin = child.stdin.take();
        let captured = Captured::default();
        captured.pump(child.stdout.take(), true);
        captured.pump(child.stderr.take(), false);
        Ok((
            Self {
                child,
                #[cfg(windows)]
                tree,
            },
            stdin,
            captured,
        ))
    }

    fn try_exit(&mut self) -> Option<Option<i32>> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code()),
            Ok(None) => None,
            Err(_) => Some(None),
        }
    }

    /// Ends the command and everything it started.
    fn end(&mut self) {
        #[cfg(windows)]
        self.tree.end(Duration::from_secs(5));
        #[cfg(unix)]
        {
            // SAFETY: signalling the process group this command leads.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(Into::into)
}

#[derive(Default)]
struct Streams {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    dropped: bool,
}

/// Output of a command, filled by reader threads.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Streams>>);

impl Captured {
    fn pump(&self, from: Option<impl Read + Send + 'static>, stdout: bool) {
        let Some(mut from) = from else { return };
        let me = self.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = from.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut s = me.0.lock().unwrap();
                let target = if stdout { &mut s.stdout } else { &mut s.stderr };
                target.extend_from_slice(&buf[..n]);
                if target.len() > BUFFER_LIMIT {
                    let cut = target.len() - BUFFER_LIMIT;
                    target.drain(..cut);
                    s.dropped = true;
                }
            }
        });
    }

    /// Takes what came so far, each stream cut to its last 64 KiB.
    fn take(&self) -> (String, String, bool) {
        let mut s = self.0.lock().unwrap();
        let cut = |v: &mut Vec<u8>| -> (String, bool) {
            let truncated = v.len() > OUTPUT_LIMIT;
            let start = v.len().saturating_sub(OUTPUT_LIMIT);
            let text = String::from_utf8_lossy(&v[start..]).to_string();
            v.clear();
            (text, truncated)
        };
        let (out, t1) = cut(&mut s.stdout);
        let (err, t2) = cut(&mut s.stderr);
        let truncated = t1 || t2 || std::mem::take(&mut s.dropped);
        (out, err, truncated)
    }

    fn is_empty(&self) -> bool {
        let s = self.0.lock().unwrap();
        s.stdout.is_empty() && s.stderr.is_empty()
    }
}

struct Background {
    running: Running,
    stdin: Option<ChildStdin>,
    captured: Captured,
    command: String,
    started: Instant,
    exit: Option<Option<i32>>,
}

/// The commands of one Environment's session.
pub struct Shell {
    enabled: Arc<AtomicBool>,
    history: Arc<History>,
    env_id: String,
    lock: Arc<ComputerLock>,
    label: String,
    procs: Mutex<HashMap<String, Background>>,
    next: AtomicU64,
}

impl Shell {
    pub fn new(
        enabled: Arc<AtomicBool>,
        history: Arc<History>,
        env_id: String,
        lock: Arc<ComputerLock>,
        label: String,
    ) -> Self {
        Self {
            enabled,
            history,
            env_id,
            lock,
            label,
            procs: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn record(&self, command: &str, exit: Option<i32>, ms: u64) {
        let command: String = command.chars().take(HISTORY_COMMAND_LIMIT).collect();
        self.history.append(HistoryEntry {
            command: Some(command),
            exit_code: exit,
            ok: Some(exit == Some(0)),
            ms: Some(ms),
            ..HistoryEntry::new(&self.env_id, Event::Command)
        });
    }

    /// Handles one of the five tools; returns the `tools/call` result.
    pub fn call(&self, name: &str, args: &Value) -> Value {
        if !self.enabled() {
            return text_result(
                "Commands on this computer are off for this Environment. The computer's owner can turn them on in Lazurio Agent Desktop.",
                true,
            );
        }
        self.reap();
        match name {
            TOOL_RUN => self.run(args),
            TOOL_START => self.start(args),
            TOOL_READ => self.read(args),
            TOOL_WRITE => self.write(args),
            TOOL_STOP => self.stop(args),
            other => text_result(format!("Unknown tool {other}"), true),
        }
    }

    fn command_of(args: &Value) -> Result<String, Value> {
        let command = args["command"].as_str().unwrap_or("").trim().to_string();
        if command.is_empty() {
            return Err(text_result("command is empty", true));
        }
        if let Some(why) = refusal(&command) {
            return Err(text_result(why, true));
        }
        Ok(command)
    }

    fn run(&self, args: &Value) -> Value {
        let command = match Self::command_of(args) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let timeout = Duration::from_secs(
            args["timeout_s"]
                .as_u64()
                .unwrap_or(RUN_TIMEOUT_DEFAULT)
                .clamp(1, RUN_TIMEOUT_MAX),
        );
        let started = Instant::now();
        let (mut running, stdin, captured) = match Running::spawn(&command, args) {
            Ok(r) => r,
            Err(e) => return text_result(e, true),
        };
        drop(stdin);
        let exit = loop {
            if let Some(code) = running.try_exit() {
                break Some(code);
            }
            if started.elapsed() >= timeout || !self.enabled() {
                running.end();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Let the readers catch the last bytes.
        std::thread::sleep(Duration::from_millis(50));
        let (stdout, stderr, truncated) = captured.take();
        let code = exit.flatten();
        self.record(&command, code, started.elapsed().as_millis() as u64);
        let mut result = json!({
            "exitCode": code,
            "timedOut": exit.is_none(),
            "stdout": stdout,
            "stderr": stderr,
            "truncated": truncated,
        });
        if code != Some(0) {
            if let Some(hint) = admin_hint(&stderr) {
                result["hint"] = json!(hint);
            }
        }
        text_result(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
            false,
        )
    }

    fn start(&self, args: &Value) -> Value {
        let command = match Self::command_of(args) {
            Ok(c) => c,
            Err(e) => return e,
        };
        if self.procs.lock().unwrap().len() >= MAX_PROCESSES {
            return text_result(
                format!(
                    "At most {MAX_PROCESSES} started commands per Environment; stop one first."
                ),
                true,
            );
        }
        let (running, stdin, captured) = match Running::spawn(&command, args) {
            Ok(r) => r,
            Err(e) => return text_result(e, true),
        };
        let id = format!("p{}", self.next.fetch_add(1, Ordering::SeqCst));
        // A started command keeps the computer for this Environment.
        let _ = self.lock.try_begin(&self.env_id, &self.label);
        self.procs.lock().unwrap().insert(
            id.clone(),
            Background {
                running,
                stdin,
                captured,
                command,
                started: Instant::now(),
                exit: None,
            },
        );
        text_result(json!({ "id": id }).to_string(), false)
    }

    fn read(&self, args: &Value) -> Value {
        let id = args["id"].as_str().unwrap_or("").to_string();
        let wait = Duration::from_secs(args["wait_s"].as_u64().unwrap_or(0).min(READ_WAIT_MAX));
        let deadline = Instant::now() + wait;
        loop {
            let ready = {
                let mut procs = self.procs.lock().unwrap();
                let Some(p) = procs.get_mut(&id) else {
                    return text_result(format!("No started command {id}"), true);
                };
                if p.exit.is_none() {
                    p.exit = p.running.try_exit();
                }
                p.exit.is_some() || !p.captured.is_empty()
            };
            if ready || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut procs = self.procs.lock().unwrap();
        let Some(p) = procs.get_mut(&id) else {
            return text_result(format!("No started command {id}"), true);
        };
        let (stdout, stderr, truncated) = p.captured.take();
        let finished = p.exit;
        let result = json!({
            "id": id,
            "running": finished.is_none(),
            "exitCode": finished.flatten(),
            "stdout": stdout,
            "stderr": stderr,
            "truncated": truncated,
        });
        if let Some(code) = finished {
            let p = procs.remove(&id).expect("present");
            drop(procs);
            self.finished(&p.command, code, p.started);
        }
        text_result(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
            false,
        )
    }

    fn write(&self, args: &Value) -> Value {
        let id = args["id"].as_str().unwrap_or("");
        let input = args["input"].as_str().unwrap_or("");
        let mut procs = self.procs.lock().unwrap();
        let Some(p) = procs.get_mut(id) else {
            return text_result(format!("No started command {id}"), true);
        };
        let written = p.stdin.as_mut().is_some_and(|stdin| {
            stdin
                .write_all(input.as_bytes())
                .and_then(|_| stdin.flush())
                .is_ok()
        });
        if written {
            text_result(json!({"id": id, "written": input.len()}).to_string(), false)
        } else {
            text_result(format!("The input of {id} is closed"), true)
        }
    }

    fn stop(&self, args: &Value) -> Value {
        let id = args["id"].as_str().unwrap_or("");
        let p = self.procs.lock().unwrap().remove(id);
        let Some(mut p) = p else {
            return text_result(format!("No started command {id}"), true);
        };
        let code = match p.running.try_exit() {
            Some(code) => code,
            None => {
                p.running.end();
                None
            }
        };
        self.finished(&p.command, code, p.started);
        text_result(
            json!({"id": id, "stopped": true, "exitCode": code}).to_string(),
            false,
        )
    }

    fn finished(&self, command: &str, code: Option<i32>, started: Instant) {
        self.record(command, code, started.elapsed().as_millis() as u64);
        self.lock.end_call(&self.env_id);
    }

    /// Notes started commands that ended on their own (their output stays readable).
    fn reap(&self) {
        let mut procs = self.procs.lock().unwrap();
        for p in procs.values_mut() {
            if p.exit.is_none() {
                p.exit = p.running.try_exit();
            }
        }
    }

    /// Whether any started command is still running.
    pub fn busy(&self) -> bool {
        self.reap();
        self.procs
            .lock()
            .unwrap()
            .values()
            .any(|p| p.exit.is_none())
    }

    /// Ends every started command: Stop, switch off or the session's end.
    pub fn end_all(&self) {
        let procs: Vec<Background> = self.procs.lock().unwrap().drain().map(|(_, p)| p).collect();
        for mut p in procs {
            let code = match p.running.try_exit() {
                Some(code) => code,
                None => {
                    p.running.end();
                    None
                }
            };
            self.finished(&p.command, code, p.started);
        }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.end_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(dir: &std::path::Path, on: bool) -> Shell {
        Shell::new(
            Arc::new(AtomicBool::new(on)),
            Arc::new(History::new(dir.join("history.jsonl"))),
            "env".into(),
            Arc::new(ComputerLock::default()),
            "Env".into(),
        )
    }

    fn body(v: &Value) -> Value {
        serde_json::from_str(v["content"][0]["text"].as_str().unwrap())
            .unwrap_or(json!(v["content"][0]["text"]))
    }

    #[cfg(unix)]
    #[test]
    fn runs_a_command_and_keeps_only_command_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let s = shell(dir.path(), true);
        let r = s.call(
            TOOL_RUN,
            &json!({"command": "echo secret-output; echo oops >&2; exit 3"}),
        );
        assert_eq!(r["isError"], false);
        let b = body(&r);
        assert_eq!(b["exitCode"], 3);
        assert_eq!(b["stdout"], "secret-output\n");
        assert_eq!(b["stderr"], "oops\n");
        let history = std::fs::read_to_string(dir.path().join("history.jsonl")).unwrap();
        assert!(history.contains("\"exit_code\":3"));
        assert!(
            history.contains("echo secret-output"),
            "the command itself is recorded"
        );
        assert!(
            !history.contains("oops\\n") && !history.contains("\"stdout\""),
            "never the output"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ends_a_command_at_its_timeout_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let s = shell(dir.path(), true);
        let started = Instant::now();
        let b = body(&s.call(
            TOOL_RUN,
            &json!({"command": "sleep 30 & sleep 30; wait", "timeout_s": 1}),
        ));
        assert_eq!(b["timedOut"], true);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn starts_reads_writes_and_stops_a_long_command() {
        let dir = tempfile::tempdir().unwrap();
        let s = shell(dir.path(), true);
        let id = body(&s.call(
            TOOL_START,
            &json!({"command": "while read line; do echo got:$line; done"}),
        ))["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(s.busy());
        s.call(TOOL_WRITE, &json!({"id": id, "input": "hello\n"}));
        let b = body(&s.call(TOOL_READ, &json!({"id": id, "wait_s": 5})));
        assert_eq!(b["stdout"], "got:hello\n");
        assert_eq!(b["running"], true);
        let b = body(&s.call(TOOL_STOP, &json!({"id": id})));
        assert_eq!(b["stopped"], true);
        assert!(!s.busy());
        assert_eq!(s.call(TOOL_READ, &json!({"id": id}))["isError"], true);
    }

    #[test]
    fn nothing_runs_while_the_switch_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let s = shell(dir.path(), false);
        let r = s.call(TOOL_RUN, &json!({"command": "echo hi"}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("off"));
    }

    #[test]
    fn explains_when_administrator_rights_are_needed() {
        assert!(admin_hint("sudo: a terminal is required to read the password").is_some());
        assert!(admin_hint("The requested operation requires elevation.").is_some());
        assert!(admin_hint("all good").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn runs_powershell_without_a_uac_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let s = shell(dir.path(), true);
        let b = body(&s.call(TOOL_RUN, &json!({"command": "Write-Output hi; exit 4"})));
        assert_eq!(b["exitCode"], 4);
        assert_eq!(b["stdout"].as_str().unwrap().trim(), "hi");
        let r = s.call(
            TOOL_RUN,
            &json!({"command": "Start-Process notepad -Verb RunAs"}),
        );
        assert_eq!(r["isError"], true);
    }
}
