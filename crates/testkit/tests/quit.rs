//! Quitting never waits for a channel without a limit.
//!
//! macOS, 13 November 2026: Quit from the tray hung for good. The main thread
//! ended the channels and joined their threads. One channel, ending its
//! session, released the computer and told the app, and the app redraws its
//! tray on the main thread and waits for that. These tests play the app's
//! main thread with an event loop of their own and a change listener that
//! waits for it the way Tauri's tray calls do.

mod common;

use common::{lines, state_of, wait, Setup};
use lazurio_agent_desktop_core::channel::{State, Timing};
use lazurio_agent_desktop_core::history::Event;
use lazurio_agent_desktop_testkit::{Approval, EnvOptions, FakeEnvironment, FakeLaunchpad};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

type Task = Box<dyn FnOnce() + Send>;

/// The app's main thread: runs posted tasks one after another.
struct MainThread {
    tasks: mpsc::Sender<Task>,
    id: ThreadId,
}

impl MainThread {
    fn start() -> Arc<Self> {
        let (tasks, inbox) = mpsc::channel::<Task>();
        let thread = thread::Builder::new()
            .name("main".into())
            .spawn(move || {
                for task in inbox {
                    task();
                }
            })
            .unwrap();
        Arc::new(Self {
            tasks,
            id: thread.thread().id(),
        })
    }

    /// What Tauri's `TrayIcon::set_icon` does: runs `task` on the main thread
    /// and waits until it ran there; on the main thread itself it runs at once.
    fn run_and_wait(&self, task: impl FnOnce() + Send + 'static) {
        if thread::current().id() == self.id {
            return task();
        }
        let (done, finished) = mpsc::channel();
        let task: Task = Box::new(move || {
            task();
            let _ = done.send(());
        });
        if self.tasks.send(task).is_ok() {
            let _ = finished.recv();
        }
    }

    /// Runs `task` on the main thread; false when it did not finish within `limit`.
    fn run_within(&self, limit: Duration, task: impl FnOnce() + Send + 'static) -> bool {
        let (done, finished) = mpsc::channel();
        let task: Task = Box::new(move || {
            task();
            let _ = done.send(());
        });
        self.tasks.send(task).is_ok() && finished.recv_timeout(limit).is_ok()
    }
}

#[test]
fn quitting_while_a_channel_waits_for_the_main_thread_does_not_hang() {
    let dir = tempfile::tempdir().unwrap();
    // The agent calls its tools and keeps the session, so its Environment
    // holds the computer when the owner quits.
    let env = FakeEnvironment::start(
        EnvOptions {
            hold: Duration::from_secs(120),
            ..EnvOptions::default()
        },
        dir.path().join("agent.jsonl"),
    );
    let lp = common::launchpad_for(&env, "test-computer", Some("Test Environment"));
    let main = MainThread::start();
    let redraws = Arc::new(AtomicUsize::new(0));
    let on_change = {
        let main = main.clone();
        let redraws = redraws.clone();
        Arc::new(move || {
            let redraws = redraws.clone();
            main.run_and_wait(move || {
                redraws.fetch_add(1, Ordering::SeqCst);
            });
        })
    };
    let (manager, _) = common::open_with(dir.path(), Setup::default(), on_change);
    let view = common::pair(&manager, &lp.origin);
    assert!(
        wait(20, || lines(&env.out) >= 1),
        "the agent did not finish its calls"
    );
    assert!(
        manager
            .controlled_by()
            .is_some_and(|holder| holder.env_id == view.id),
        "the agent's Environment should hold the computer"
    );

    // Quit on the main thread, as the tray's Quit does.
    let before = redraws.load(Ordering::SeqCst);
    let quitting = manager.clone();
    let started = Instant::now();
    let quit = main.run_within(Duration::from_secs(15), move || {
        quitting.shutdown();
    });
    assert!(
        quit,
        "quitting hung: the main thread waits for the channel and the channel waits for the main thread"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "quitting took {:?}",
        started.elapsed()
    );

    // The channel ended cleanly, it was not just left behind.
    assert!(manager
        .history(50)
        .iter()
        .any(|e| e.event == Event::Disconnected && e.reason.as_deref() == Some("switched-off")));
    assert_eq!(manager.controlled_by(), None);
    // The tray hears about it once the main thread is free again.
    assert!(
        wait(5, || redraws.load(Ordering::SeqCst) > before),
        "the app was not told about the change"
    );
}

#[test]
fn quitting_does_not_wait_for_a_channel_that_is_still_connecting() {
    // Accepts the connection and never answers the WebSocket upgrade: the
    // channel sits in its handshake for the whole connect timeout.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in silent.incoming() {
            held.push(stream);
        }
    });
    let lp = FakeLaunchpad::start(Approval {
        device_id: "test-computer".into(),
        channel_url: Some(format!("ws://127.0.0.1:{port}/.lazurio/computers/attach")),
        decline: false,
        environment_name: None,
        registry: None,
    });
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = common::open(
        dir.path(),
        Setup {
            timing: Timing {
                connect_timeout: Duration::from_secs(30),
                ..common::quick_timing()
            },
            ..Setup::default()
        },
    );
    let view = common::pair(&manager, &lp.origin);
    assert!(wait(10, || state_of(&manager, &view.id) == State::Connecting));
    thread::sleep(Duration::from_millis(300));

    let started = Instant::now();
    manager.shutdown();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "quitting waited {:?} for a channel that was still connecting",
        started.elapsed()
    );
}
