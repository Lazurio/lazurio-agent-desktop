//! One agent at a time on the whole computer (contract v1, section 1, and
//! ADR 0006 point 4 of Lazurio App).
//!
//! The lock has two layers. Inside this app, the hold of contract v1: while a
//! tool from one Environment runs, or finished less than 20 seconds ago, every
//! tool call from another Environment is refused at once with an MCP tool
//! error. The Environment that holds the computer keeps calling; the hold ends
//! 20 seconds after its last call or when its session ends.
//!
//! Around it, the device lock of the person's session: an exclusive lock
//! (`File::try_lock`) on one file that this app and Lazurio App both take
//! before they serve a tool call, so only one of them works at a time. A hold
//! takes it when it starts and lets it go when it ends; the operating system
//! lets it go when the process ends, crashes included. When the file cannot be
//! opened or locked, the call is refused: the lock fails closed.
//!
//! A thread of the lock ends a passed hold at its deadline, so a quiet app
//! never keeps the device lock from Lazurio App until its next call.

use crate::time::now_ms;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub const HOLD_AFTER_LAST_CALL: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub env_id: String,
    pub label: String,
    /// Unix milliseconds since this Environment holds the computer.
    pub since_ms: u64,
}

/// Why a tool call cannot start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Busy {
    /// An agent of another Environment of this app holds the computer.
    Environment(Holder),
    /// Another process holds the device lock, for example Lazurio App, or the
    /// lock file cannot be used at all.
    OtherApplication,
}

impl Busy {
    /// The tool error an agent gets, in Czech and English.
    pub fn message(&self) -> String {
        match self {
            Busy::Environment(holder) => busy_message(holder),
            Busy::OtherApplication => OTHER_APPLICATION.to_string(),
        }
    }
}

/// The same words as Lazurio App's `Busy::OtherApplication`.
const OTHER_APPLICATION: &str = "Počítač teď ovládá agent z jiné aplikace Lazurio. Zkuste to, až skončí. (The computer is in use by an agent from another Lazurio app. Try again when it is done.)";

struct Hold {
    holder: Holder,
    active: u32,
    last: Instant,
    /// The open lock file: this process holds the device lock while it lives.
    _device_lock: File,
}

struct State {
    hold: Option<Hold>,
    /// The lock was dropped: its thread ends.
    closed: bool,
}

pub type LockListener = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    state: Mutex<State>,
    /// Woken on every change, so the thread waits for the right deadline.
    changed: Condvar,
    linger: Duration,
    listener: Mutex<Option<LockListener>>,
    /// The device lock file; `None` when the system gives it no place.
    path: Option<PathBuf>,
    /// Whether a lock file that cannot be used was already logged.
    logged: AtomicBool,
}

pub struct ComputerLock {
    inner: Arc<Inner>,
}

impl Default for ComputerLock {
    fn default() -> Self {
        Self::with_linger(HOLD_AFTER_LAST_CALL)
    }
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere never leaves the lock unusable: its state is valid at every step.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ComputerLock {
    /// The lock with the device lock of the person's session
    /// ([`device_lock_path`]).
    pub fn with_linger(linger: Duration) -> Self {
        Self::at(device_lock_path(), linger)
    }

    /// The lock with the device lock file at `path` (tests).
    pub fn with_device_lock(path: PathBuf, linger: Duration) -> Self {
        Self::at(Some(path), linger)
    }

    pub(crate) fn at(path: Option<PathBuf>, linger: Duration) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                hold: None,
                closed: false,
            }),
            changed: Condvar::new(),
            linger,
            listener: Mutex::new(None),
            path,
            logged: AtomicBool::new(false),
        });
        let worker = inner.clone();
        std::thread::Builder::new()
            .name("lazurio-lock".into())
            .spawn(move || worker.expire_on_deadline())
            .expect("spawn lock thread");
        Self { inner }
    }

    /// Called whenever the holder changes, also when a hold ends on its own.
    /// It runs on the thread that changed it and must return at once.
    pub fn set_listener(&self, listener: LockListener) {
        *guard(&self.inner.listener) = Some(listener);
    }

    /// A tool call from `env_id` starts. Refused when another Environment
    /// holds the computer, or when another process holds the device lock or
    /// it cannot be taken.
    pub fn try_begin(&self, env_id: &str, label: &str) -> Result<(), Busy> {
        let inner = &self.inner;
        let (result, changed) = {
            let mut state = guard(&inner.state);
            match state.hold.as_mut() {
                Some(h) if h.holder.env_id == env_id => {
                    h.active += 1;
                    h.last = Instant::now();
                    (Ok(()), false)
                }
                Some(h) if !inner.expired(h) => {
                    return Err(Busy::Environment(h.holder.clone()));
                }
                _ => {
                    // A passed hold lets go of its file before a new one opens it,
                    // or this process would lock itself out.
                    let ended = state.hold.take().is_some();
                    match inner.take_device_lock() {
                        Ok(file) => {
                            state.hold = Some(Hold {
                                holder: Holder {
                                    env_id: env_id.to_string(),
                                    label: label.to_string(),
                                    since_ms: now_ms(),
                                },
                                active: 1,
                                last: Instant::now(),
                                _device_lock: file,
                            });
                            (Ok(()), true)
                        }
                        Err(busy) => (Err(busy), ended),
                    }
                }
            }
        };
        inner.changed.notify_all();
        if changed {
            inner.notify();
        }
        result
    }

    /// A tool call from `env_id` finished.
    pub fn end_call(&self, env_id: &str) {
        {
            let mut state = guard(&self.inner.state);
            if let Some(h) = state.hold.as_mut().filter(|h| h.holder.env_id == env_id) {
                h.active = h.active.saturating_sub(1);
                h.last = Instant::now();
            }
        }
        self.inner.changed.notify_all();
    }

    /// The session of `env_id` ended: it no longer holds the computer, and
    /// the device lock is free.
    pub fn release(&self, env_id: &str) {
        let released = {
            let mut state = guard(&self.inner.state);
            if state
                .hold
                .as_ref()
                .is_some_and(|h| h.holder.env_id == env_id)
            {
                state.hold = None;
                true
            } else {
                false
            }
        };
        self.inner.changed.notify_all();
        if released {
            self.inner.notify();
        }
    }

    /// Who holds the computer now, if anyone.
    pub fn holder(&self) -> Option<Holder> {
        let state = guard(&self.inner.state);
        state
            .hold
            .as_ref()
            .filter(|h| !self.inner.expired(h))
            .map(|h| h.holder.clone())
    }
}

impl Drop for ComputerLock {
    fn drop(&mut self) {
        // Under the state's mutex, so the thread either waits (and gets the
        // wake-up) or sees `closed` before it waits again.
        guard(&self.inner.state).closed = true;
        self.inner.changed.notify_all();
    }
}

impl Inner {
    fn notify(&self) {
        let listener = guard(&self.listener).clone();
        if let Some(l) = listener {
            l();
        }
    }

    fn expired(&self, hold: &Hold) -> bool {
        hold.active == 0 && hold.last.elapsed() >= self.linger
    }

    /// When the hold ends if no call comes: `None` while a call runs or nobody holds.
    fn deadline(&self, state: &State) -> Option<Instant> {
        state
            .hold
            .as_ref()
            .filter(|h| h.active == 0)
            .map(|h| h.last + self.linger)
    }

    /// The lock's thread: ends a passed hold at its deadline and lets go of
    /// the device lock, whether or not another call comes.
    fn expire_on_deadline(&self) {
        let mut state = guard(&self.state);
        loop {
            if state.closed {
                return;
            }
            let now = Instant::now();
            match self.deadline(&state) {
                Some(deadline) if deadline <= now => {
                    state.hold = None;
                    drop(state);
                    self.notify();
                    state = guard(&self.state);
                }
                Some(deadline) => {
                    state = self
                        .changed
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                None => {
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }

    /// The device lock for a new hold, or why a call cannot start. A lock file
    /// that cannot be used is logged once, without its path.
    fn take_device_lock(&self) -> Result<File, Busy> {
        let Some(path) = &self.path else {
            if !self.logged.swap(true, Ordering::SeqCst) {
                eprintln!(
                    "Lazurio Agent Desktop: the system gives no place for the device lock; every tool call is refused."
                );
            }
            return Err(Busy::OtherApplication);
        };
        take_device_lock(path).map_err(|error| {
            if error.kind() != io::ErrorKind::WouldBlock
                && !self.logged.swap(true, Ordering::SeqCst)
            {
                eprintln!(
                    "Lazurio Agent Desktop: the device lock cannot be used ({:?}); tool calls are refused.",
                    error.kind()
                );
            }
            Busy::OtherApplication
        })
    }
}

/// Opens the device lock file and takes its exclusive lock without waiting,
/// as Lazurio App does.
fn take_device_lock(path: &Path) -> io::Result<File> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "another process holds the device lock",
        )),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Where the device lock of the person's session is, the same for Lazurio
/// App and this app: `~/Library/Application Support/io.lazurio.device/agent.lock`
/// on macOS, `%LOCALAPPDATA%\io.lazurio.device\agent.lock` on Windows, and
/// under `$XDG_STATE_HOME` (or `~/.local/state`) elsewhere. `None` when the
/// system gives no such directory.
pub fn device_lock_path() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
    }?;
    Some(base.join("io.lazurio.device").join("agent.lock"))
}

/// The tool error another Environment's agent gets, in Czech and English.
pub fn busy_message(holder: &Holder) -> String {
    format!(
        "Počítač teď ovládá agent z {0}. Zkuste to, až skončí. (The computer is in use by an agent from {0}. Try again when it is done.)",
        holder.label
    )
}

/// Test processes and device locks (Lazurio/lazurio-app#29 and #30).
///
/// A child process started in one test thread inherits every descriptor of
/// this process until its `exec`, and `flock` belongs to the open file, so for
/// that moment the child also holds any device lock another test thread has
/// taken. Tests that start a process take [`test_guard::spawning`]; tests that
/// take a device lock in this process take [`test_guard::in_process`], so no
/// process is ever started while they hold one.
#[cfg(test)]
pub(crate) mod test_guard {
    use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

    static FORKS: RwLock<()> = RwLock::new(());

    pub(crate) fn in_process() -> RwLockReadGuard<'static, ()> {
        FORKS.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn spawning() -> RwLockWriteGuard<'static, ()> {
        FORKS.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// A fresh directory for one test's lock file.
    pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir()
            .join("lazurio-agent-desktop-tests")
            .join(format!("lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("a scratch directory");
        directory
    }
}

#[cfg(test)]
mod tests {
    use super::test_guard::{in_process, scratch, spawning};
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    fn lock_in(name: &str, linger: Duration) -> ComputerLock {
        ComputerLock::with_device_lock(scratch(name).join("agent.lock"), linger)
    }

    #[test]
    fn one_environment_at_a_time() {
        let _forks = in_process();
        let lock = lock_in("one", Duration::from_millis(100));
        lock.try_begin("a", "Prostředí A").unwrap();
        lock.try_begin("a", "Prostředí A").unwrap();
        let refused = lock.try_begin("b", "Prostředí B").unwrap_err();
        let Busy::Environment(holder) = &refused else {
            panic!("refused for {refused:?}");
        };
        assert_eq!(holder.label, "Prostředí A");
        assert!(busy_message(holder).contains("ovládá agent z Prostředí A"));
        assert_eq!(refused.message(), busy_message(holder));
        lock.end_call("a");
        lock.end_call("a");
        // Still held right after the last call...
        assert!(lock.try_begin("b", "Prostředí B").is_err());
        std::thread::sleep(Duration::from_millis(150));
        // ...and free once the hold has passed.
        assert_eq!(lock.holder(), None);
        lock.try_begin("b", "Prostředí B").unwrap();
        assert_eq!(lock.holder().unwrap().env_id, "b");
    }

    #[test]
    fn a_running_call_keeps_the_hold_and_release_ends_it() {
        let _forks = in_process();
        let lock = lock_in("running", Duration::from_millis(10));
        lock.try_begin("a", "A").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            lock.try_begin("b", "B").is_err(),
            "a running call must keep the computer"
        );
        lock.release("a");
        lock.try_begin("b", "B").unwrap();
        lock.release("a");
        assert_eq!(lock.holder().unwrap().env_id, "b");
    }

    #[test]
    fn two_locks_on_one_file_exclude_each_other() {
        let _forks = in_process();
        let path = scratch("two").join("agent.lock");
        let first = ComputerLock::with_device_lock(path.clone(), HOLD_AFTER_LAST_CALL);
        let second = ComputerLock::with_device_lock(path, HOLD_AFTER_LAST_CALL);
        first.try_begin("a", "A").expect("the first takes it");
        assert_eq!(second.try_begin("b", "B"), Err(Busy::OtherApplication));
        assert_eq!(
            second.try_begin("a", "A"),
            Err(Busy::OtherApplication),
            "the same Environment id in another app is another app"
        );
        assert_eq!(
            second.holder(),
            None,
            "the other app holds, not one of ours"
        );
        first.release("a");
        second
            .try_begin("b", "B")
            .expect("the second after the first released");
        assert_eq!(first.try_begin("a", "A"), Err(Busy::OtherApplication));
    }

    #[test]
    fn fails_closed_when_the_lock_file_cannot_be_opened() {
        let _forks = in_process();
        let directory = scratch("closed");
        let not_a_folder = directory.join("not-a-folder");
        std::fs::write(&not_a_folder, b"").unwrap();
        let lock = ComputerLock::with_device_lock(
            not_a_folder.join("io.lazurio.device").join("agent.lock"),
            HOLD_AFTER_LAST_CALL,
        );
        assert_eq!(lock.try_begin("a", "A"), Err(Busy::OtherApplication));
        assert_eq!(lock.holder(), None);
    }

    #[test]
    fn fails_closed_without_a_place_for_the_lock() {
        let lock = ComputerLock::at(None, HOLD_AFTER_LAST_CALL);
        assert_eq!(lock.try_begin("a", "A"), Err(Busy::OtherApplication));
        assert_eq!(lock.try_begin("a", "A"), Err(Busy::OtherApplication));
    }

    #[test]
    fn the_device_lock_lives_where_lazurio_app_keeps_it() {
        // Byte for byte the rule of Lazurio App's `device_lock_path()`.
        let expected = if cfg!(target_os = "macos") {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join("Library/Application Support"))
        } else if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
                })
        }
        .map(|base| base.join("io.lazurio.device").join("agent.lock"));
        assert!(expected.is_some(), "the test machine has a home");
        assert_eq!(device_lock_path(), expected);
    }

    #[test]
    fn the_other_application_message_is_the_one_of_lazurio_app() {
        assert_eq!(
            Busy::OtherApplication.message(),
            "Počítač teď ovládá agent z jiné aplikace Lazurio. Zkuste to, až skončí. (The computer is in use by an agent from another Lazurio app. Try again when it is done.)"
        );
    }

    /// Run by the tests below in a child process: takes the device lock at
    /// the path in `LAZURIO_DEVICE_LOCK_CHILD`, says so on its output, and
    /// holds it until its input ends.
    #[test]
    #[ignore = "a helper process for the cross-process tests"]
    fn device_lock_child() {
        let Some(path) = std::env::var_os("LAZURIO_DEVICE_LOCK_CHILD") else {
            return;
        };
        let lock = ComputerLock::with_device_lock(PathBuf::from(path), HOLD_AFTER_LAST_CALL);
        lock.try_begin("child", "Child")
            .expect("the child takes the lock");
        println!("LOCKED");
        std::io::stdout().flush().expect("flush");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }

    fn child_command(path: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("the test binary"));
        command
            .args([
                "--exact",
                "lock::tests::device_lock_child",
                "--ignored",
                "--nocapture",
            ])
            .env("LAZURIO_DEVICE_LOCK_CHILD", path);
        command
    }

    /// A child process that holds the device lock at `path` until its input ends.
    fn spawn_child(path: &Path) -> std::process::Child {
        let mut child = child_command(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the child starts");
        let stdout = child.stdout.take().expect("the child's output");
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next() {
                Some(Ok(line)) if line.trim() == "LOCKED" => break,
                Some(Ok(_)) => {}
                other => panic!("the child never took the lock: {other:?}"),
            }
        }
        // Keep reading the child's output so it never writes into a closed pipe.
        std::thread::spawn(move || lines.for_each(drop));
        child
    }

    /// Whether a fresh child process can take the device lock at `path`.
    fn another_process_can_take(path: &Path) -> bool {
        let output = child_command(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("the child runs");
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("LOCKED")
    }

    #[test]
    fn another_process_holding_the_device_lock_refuses_every_call_until_it_ends() {
        let _forks = spawning();
        let path = scratch("process").join("agent.lock");
        let mut child = spawn_child(&path);
        let lock = ComputerLock::with_device_lock(path, HOLD_AFTER_LAST_CALL);
        assert_eq!(lock.try_begin("a", "A"), Err(Busy::OtherApplication));
        assert_eq!(lock.try_begin("b", "B"), Err(Busy::OtherApplication));
        assert_eq!(lock.holder(), None);
        drop(child.stdin.take());
        child.wait().expect("the child ends");
        lock.try_begin("a", "A")
            .expect("free once the other process ended");
    }

    #[test]
    fn a_killed_process_releases_the_device_lock() {
        let _forks = spawning();
        let path = scratch("killed").join("agent.lock");
        let mut child = spawn_child(&path);
        let lock = ComputerLock::with_device_lock(path, HOLD_AFTER_LAST_CALL);
        assert_eq!(lock.try_begin("a", "A"), Err(Busy::OtherApplication));
        child.kill().expect("the child is killed");
        child.wait().expect("the child ends");
        lock.try_begin("a", "A")
            .expect("free once the holder was killed");
    }

    #[test]
    fn while_this_process_holds_it_another_process_cannot_take_it() {
        let _forks = spawning();
        let path = scratch("reverse").join("agent.lock");
        let lock = ComputerLock::with_device_lock(path.clone(), HOLD_AFTER_LAST_CALL);
        lock.try_begin("a", "A").expect("this process takes it");
        assert!(!another_process_can_take(&path), "held by this process");
        lock.end_call("a");
        assert!(
            !another_process_can_take(&path),
            "still held during the hold after the last call"
        );
        lock.release("a");
        assert!(another_process_can_take(&path), "free after release");
    }

    #[test]
    fn a_quiet_utility_releases_the_device_lock_at_the_deadline() {
        let _forks = spawning();
        let path = scratch("deadline").join("agent.lock");
        let lock = ComputerLock::with_device_lock(path.clone(), Duration::from_millis(50));
        let (changed, changes) = mpsc::channel();
        lock.set_listener(Arc::new(move || {
            let _ = changed.send(());
        }));
        lock.try_begin("a", "A").expect("this process takes it");
        changes
            .recv_timeout(Duration::from_secs(5))
            .expect("told that A holds the computer");
        lock.end_call("a");
        // Nothing calls this lock again: only its own thread can end the hold.
        changes
            .recv_timeout(Duration::from_secs(30))
            .expect("the hold ends on its own");
        assert_eq!(lock.holder(), None);
        assert!(another_process_can_take(&path), "the file is free");
        drop(lock);
    }
}
