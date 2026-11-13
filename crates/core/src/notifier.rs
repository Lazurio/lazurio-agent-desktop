//! Change notifications for the user interface, delivered off the channel threads.
//!
//! The app redraws its tray icon on its main thread and waits there until the
//! redraw is done. Quitting runs on that same main thread and waits for the
//! channels to end. A channel thread that called the app directly while it
//! ended its session therefore waited for the main thread, which waited for
//! the channel: Quit hung for good (macOS, 13 November 2026).
//!
//! Channels and the computer-wide lock only mark here that something changed
//! and go on at once. A thread of its own calls the listener and folds a burst
//! of changes into one call, so whatever the listener waits for, a channel
//! never waits for it.

use crate::manager::ChangeListener;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

#[derive(Default)]
struct Flags {
    pending: bool,
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    flags: Mutex<Flags>,
    cv: Condvar,
}

impl Shared {
    fn flags(&self) -> MutexGuard<'_, Flags> {
        self.flags
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct Notifier {
    shared: Arc<Shared>,
}

impl Notifier {
    pub fn spawn(listener: ChangeListener) -> Self {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("lazurio-notify".into())
            .spawn(move || loop {
                {
                    let mut flags = worker.flags();
                    while !flags.pending && !flags.stopped {
                        flags = worker
                            .cv
                            .wait(flags)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    if flags.stopped {
                        return;
                    }
                    flags.pending = false;
                }
                listener();
            })
            .expect("spawn notifier thread");
        Self { shared }
    }

    /// Marks a change and returns at once; never waits for the listener.
    pub fn notify(&self) {
        self.shared.flags().pending = true;
        self.shared.cv.notify_one();
    }

    /// Ends the thread once the listener call it may be in returns. Nobody
    /// waits for that.
    pub fn stop(&self) {
        self.shared.flags().stopped = true;
        self.shared.cv.notify_one();
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn wait_for(calls: &AtomicUsize, n: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) < n {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[test]
    fn notify_never_waits_for_a_blocked_listener() {
        let (release, blocked) = mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let notifier = Notifier::spawn(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            // Like a tray redraw while the main thread is busy elsewhere.
            let _ = blocked.lock().unwrap().recv();
        }));
        notifier.notify();
        assert!(wait_for(&calls, 1), "the listener was not called");
        // The listener is blocked now; marking changes still returns at once.
        let started = Instant::now();
        for _ in 0..1000 {
            notifier.notify();
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        // The burst behind the blocked call folds into one more call.
        release.send(()).unwrap();
        release.send(()).unwrap();
        assert!(wait_for(&calls, 2), "the burst was lost");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stop_ends_the_thread_without_more_calls() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let notifier = Notifier::spawn(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        notifier.stop();
        notifier.notify();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
