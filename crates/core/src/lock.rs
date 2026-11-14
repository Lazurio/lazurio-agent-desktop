//! One agent at a time on the whole computer (contract v1, section 1).
//!
//! While a tool from one Environment runs, or finished less than 20 seconds
//! ago, every tool call from another Environment is refused at once with an
//! MCP tool error. The Environment that holds the computer keeps calling; the
//! hold ends 20 seconds after its last call or when its session ends.

use crate::time::now_ms;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const HOLD_AFTER_LAST_CALL: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub env_id: String,
    pub label: String,
    /// Unix milliseconds since this Environment holds the computer.
    pub since_ms: u64,
}

struct Hold {
    holder: Holder,
    active: u32,
    last: Instant,
}

pub type LockListener = Arc<dyn Fn() + Send + Sync>;

pub struct ComputerLock {
    hold: Mutex<Option<Hold>>,
    linger: Duration,
    listener: Mutex<Option<LockListener>>,
}

impl Default for ComputerLock {
    fn default() -> Self {
        Self::with_linger(HOLD_AFTER_LAST_CALL)
    }
}

impl ComputerLock {
    pub fn with_linger(linger: Duration) -> Self {
        Self {
            hold: Mutex::new(None),
            linger,
            listener: Mutex::new(None),
        }
    }

    /// Called whenever the holder changes.
    pub fn set_listener(&self, listener: LockListener) {
        *self.listener.lock().unwrap() = Some(listener);
    }

    fn notify(&self) {
        let listener = self.listener.lock().unwrap().clone();
        if let Some(l) = listener {
            l();
        }
    }

    fn expired(&self, hold: &Hold) -> bool {
        hold.active == 0 && hold.last.elapsed() >= self.linger
    }

    /// A tool call from `env_id` starts. Refused with the current holder when
    /// another Environment holds the computer.
    pub fn try_begin(&self, env_id: &str, label: &str) -> Result<(), Holder> {
        let changed = {
            let mut hold = self.hold.lock().unwrap();
            match hold.as_mut() {
                Some(h) if h.holder.env_id == env_id => {
                    h.active += 1;
                    h.last = Instant::now();
                    false
                }
                Some(h) if !self.expired(h) => return Err(h.holder.clone()),
                _ => {
                    *hold = Some(Hold {
                        holder: Holder {
                            env_id: env_id.to_string(),
                            label: label.to_string(),
                            since_ms: now_ms(),
                        },
                        active: 1,
                        last: Instant::now(),
                    });
                    true
                }
            }
        };
        if changed {
            self.notify();
        }
        Ok(())
    }

    /// A tool call from `env_id` finished.
    pub fn end_call(&self, env_id: &str) {
        let mut hold = self.hold.lock().unwrap();
        if let Some(h) = hold.as_mut().filter(|h| h.holder.env_id == env_id) {
            h.active = h.active.saturating_sub(1);
            h.last = Instant::now();
        }
    }

    /// The session of `env_id` ended: it no longer holds the computer.
    pub fn release(&self, env_id: &str) {
        let released = {
            let mut hold = self.hold.lock().unwrap();
            if hold.as_ref().is_some_and(|h| h.holder.env_id == env_id) {
                *hold = None;
                true
            } else {
                false
            }
        };
        if released {
            self.notify();
        }
    }

    /// Who holds the computer now, if anyone.
    pub fn holder(&self) -> Option<Holder> {
        let hold = self.hold.lock().unwrap();
        hold.as_ref()
            .filter(|h| !self.expired(h))
            .map(|h| h.holder.clone())
    }
}

/// The tool error another Environment's agent gets, in Czech and English.
pub fn busy_message(holder: &Holder) -> String {
    format!(
        "Počítač teď ovládá agent z {0}. Zkuste to, až skončí. (The computer is in use by an agent from {0}. Try again when it is done.)",
        holder.label
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_environment_at_a_time() {
        let lock = ComputerLock::with_linger(Duration::from_millis(100));
        lock.try_begin("a", "Prostředí A").unwrap();
        lock.try_begin("a", "Prostředí A").unwrap();
        let refused = lock.try_begin("b", "Prostředí B").unwrap_err();
        assert_eq!(refused.label, "Prostředí A");
        assert!(busy_message(&refused).contains("ovládá agent z Prostředí A"));
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
        let lock = ComputerLock::with_linger(Duration::from_millis(10));
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
}
