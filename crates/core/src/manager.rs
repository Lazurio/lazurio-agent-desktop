//! Paired Environments, their switches and channels: what the panel works with.

use crate::channel::{
    ChannelConfig, ChannelStatus, EngineSpec, KeyProvider, StatusListener, Supervisor, Timing,
};
use crate::history::{Event, History, HistoryEntry};
use crate::keys::DeviceKey;
use crate::keystore::SecretStore;
use crate::lock::{ComputerLock, Holder};
use crate::pairing::{PairingRequest, PairingSession};
use crate::store::{EnvironmentRecord, Store};
use crate::{launchpad, time, Error, Result};
use rand_core::{OsRng, RngCore};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct ManagerConfig {
    pub data_dir: PathBuf,
    pub engine: EngineSpec,
    pub computer_name: String,
    pub os: String,
    /// The app's own tools (status, update) served next to the engine's.
    pub app_tools: Option<crate::app_tools::AppToolsHandle>,
    /// Keep-alive and timeouts of the channels.
    pub timing: Timing,
    /// How long the computer stays with an Environment after its last tool
    /// call (contract v1: 20 s).
    pub hold_after_last_call: Duration,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentView {
    pub id: String,
    pub label: String,
    pub launchpad: String,
    pub device_id: String,
    /// The Launchpad's host, without scheme or port.
    pub host: String,
    pub enabled: bool,
    /// Another Environment's name while its agent has the computer.
    pub controlled_by: Option<String>,
    /// "Commands on this computer" is on for this Environment.
    pub commands: bool,
    pub paired_at: String,
    pub key_fingerprint: String,
    pub status: ChannelStatus,
}

/// A pairing that waits for the owner's approval in the browser.
pub struct PendingPairing {
    session: PairingSession,
    key: DeviceKey,
    launchpad: String,
    pub url: String,
    pub fingerprint: String,
}

pub type ChangeListener = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    records: Vec<EnvironmentRecord>,
    supervisors: HashMap<String, Supervisor>,
    /// The live "commands" switch each channel reads.
    commands: HashMap<String, Arc<AtomicBool>>,
}

pub struct Manager {
    cfg: ManagerConfig,
    store: Store,
    keystore: Arc<dyn SecretStore>,
    history: Arc<History>,
    on_change: ChangeListener,
    inner: Mutex<Inner>,
    lock: Arc<ComputerLock>,
}

fn account(id: &str) -> String {
    format!("env:{id}")
}

fn random_id() -> String {
    let mut raw = [0u8; 8];
    OsRng.fill_bytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

impl Manager {
    pub fn open(
        cfg: ManagerConfig,
        keystore: Arc<dyn SecretStore>,
        on_change: ChangeListener,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        // What v0 (SSH) left here: short-lived key files and known_hosts.
        let _ = std::fs::remove_dir_all(cfg.data_dir.join("run"));
        let _ = std::fs::remove_dir_all(cfg.data_dir.join("known_hosts"));
        let store = Store::new(cfg.data_dir.join("environments.json"));
        let (records, migrated) = store.load()?;
        if migrated {
            store.save(&records)?;
        }
        let lock = Arc::new(ComputerLock::with_linger(cfg.hold_after_last_call));
        {
            let on_change = on_change.clone();
            lock.set_listener(Arc::new(move || on_change()));
        }
        let manager = Arc::new(Self {
            lock,
            history: Arc::new(History::new(cfg.data_dir.join("history.jsonl"))),
            store,
            keystore,
            on_change,
            inner: Mutex::new(Inner {
                records: Vec::new(),
                supervisors: HashMap::new(),
                commands: HashMap::new(),
            }),
            cfg,
        });
        {
            let mut inner = manager.inner.lock().unwrap();
            for rec in &records {
                let flag = Arc::new(AtomicBool::new(rec.commands));
                let sup = manager.spawn_supervisor(rec, flag.clone());
                inner.supervisors.insert(rec.id.clone(), sup);
                inner.commands.insert(rec.id.clone(), flag);
            }
            inner.records = records;
        }
        Ok(manager)
    }

    pub fn history_log(&self) -> &History {
        &self.history
    }

    fn spawn_supervisor(&self, rec: &EnvironmentRecord, commands: Arc<AtomicBool>) -> Supervisor {
        let keystore = self.keystore.clone();
        let acct = account(&rec.id);
        let key: KeyProvider = Arc::new(move || {
            let seed = keystore
                .get(&acct)?
                .ok_or_else(|| Error::KeyStore("the key of this Environment is missing".into()))?;
            let seed: [u8; 32] = seed
                .as_slice()
                .try_into()
                .map_err(|_| Error::Key("stored key has the wrong length".into()))?;
            Ok(zeroize::Zeroizing::new(seed))
        });
        let on_change = self.on_change.clone();
        let listener: StatusListener = Arc::new(move |_, _| on_change());
        let cfg = ChannelConfig {
            env_id: rec.id.clone(),
            env_label: rec.label.clone(),
            channel_url: rec.channel_url.clone(),
            device_id: rec.device_id.clone(),
            engine: self.cfg.engine.clone(),
            app_tools: self.cfg.app_tools.clone(),
            lock: self.lock.clone(),
            commands,
            timing: self.cfg.timing,
        };
        Supervisor::spawn(cfg, key, self.history.clone(), listener, rec.enabled)
    }

    /// The Environment whose agent has the computer now, if any.
    pub fn controlled_by(&self) -> Option<Holder> {
        self.lock.holder()
    }

    pub fn environments(&self) -> Vec<EnvironmentView> {
        let holder = self.lock.holder();
        let inner = self.inner.lock().unwrap();
        inner
            .records
            .iter()
            .map(|r| EnvironmentView {
                id: r.id.clone(),
                label: r.label.clone(),
                launchpad: r.launchpad.clone(),
                device_id: r.device_id.clone(),
                host: url::Url::parse(&r.launchpad)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_default(),
                enabled: r.enabled,
                controlled_by: holder
                    .as_ref()
                    .filter(|h| h.env_id != r.id)
                    .map(|h| h.label.clone()),
                commands: r.commands,
                paired_at: r.paired_at.clone(),
                key_fingerprint: r.key_fingerprint.clone(),
                status: inner
                    .supervisors
                    .get(&r.id)
                    .map(|s| s.status())
                    .unwrap_or_else(|| ChannelStatus {
                        state: crate::channel::State::Off,
                        since_ms: 0,
                        tool: None,
                        app: None,
                        retry_at_ms: None,
                        reason: None,
                    }),
            })
            .collect()
    }

    /// Creates a new key and the approval URL to open in the browser.
    pub fn begin_pairing(&self, launchpad_input: &str) -> Result<PendingPairing> {
        let launchpad = launchpad::normalize(launchpad_input)?;
        let key = DeviceKey::generate()?;
        let session = PairingSession::start(&PairingRequest {
            launchpad: launchpad.clone(),
            computer_name: self.cfg.computer_name.clone(),
            os: self.cfg.os.clone(),
            public_key: key.public_openssh(),
        })?;
        Ok(PendingPairing {
            url: session.url().to_string(),
            fingerprint: key.fingerprint(),
            session,
            key,
            launchpad,
        })
    }

    /// Waits for Launchpad, then stores the key and turns the switch on: the
    /// owner has just allowed agents on this computer.
    pub fn complete_pairing(
        &self,
        pending: PendingPairing,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<EnvironmentView> {
        let PendingPairing {
            session,
            key,
            launchpad: origin,
            fingerprint,
            ..
        } = pending;
        let result = session.wait(timeout, cancel)?;
        let id = random_id();
        self.keystore.set(&account(&id), &key.seed())?;
        let rec = EnvironmentRecord {
            id: id.clone(),
            label: result
                .environment_name
                .clone()
                .unwrap_or_else(|| launchpad::label(&origin)),
            launchpad: origin.clone(),
            device_id: result.device_id,
            channel_url: result.channel_url,
            enabled: true,
            commands: false,
            paired_at: time::today(),
            key_fingerprint: fingerprint,
        };
        // Pairing the same Environment again replaces the old key.
        let replaced: Vec<String> = {
            let inner = self.inner.lock().unwrap();
            inner
                .records
                .iter()
                .filter(|r| r.launchpad == origin)
                .map(|r| r.id.clone())
                .collect()
        };
        for old in replaced {
            let _ = self.remove(&old);
        }
        let flag = Arc::new(AtomicBool::new(false));
        let sup = self.spawn_supervisor(&rec, flag.clone());
        {
            let mut inner = self.inner.lock().unwrap();
            inner.records.push(rec.clone());
            inner.supervisors.insert(id.clone(), sup);
            inner.commands.insert(id.clone(), flag);
            self.store.save(&inner.records)?;
        }
        self.history.record(&id, Event::Paired);
        (self.on_change)();
        self.environments()
            .into_iter()
            .find(|e| e.id == id)
            .ok_or(Error::UnknownEnvironment(id))
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            let rec = inner
                .records
                .iter_mut()
                .find(|r| r.id == id)
                .ok_or_else(|| Error::UnknownEnvironment(id.into()))?;
            if rec.enabled == enabled {
                return Ok(());
            }
            rec.enabled = enabled;
            self.store.save(&inner.records)?;
            if let Some(s) = inner.supervisors.get(id) {
                s.set_enabled(enabled);
            }
        }
        self.history.record(
            id,
            if enabled {
                Event::SwitchedOn
            } else {
                Event::SwitchedOff
            },
        );
        (self.on_change)();
        Ok(())
    }

    /// The owner's "commands on this computer" switch for one Environment.
    /// Turning it off ends the agent's running commands at once.
    pub fn set_commands(&self, id: &str, enabled: bool) -> Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            let rec = inner
                .records
                .iter_mut()
                .find(|r| r.id == id)
                .ok_or_else(|| Error::UnknownEnvironment(id.into()))?;
            if rec.commands == enabled {
                return Ok(());
            }
            rec.commands = enabled;
            self.store.save(&inner.records)?;
            if let Some(flag) = inner.commands.get(id) {
                flag.store(enabled, Ordering::SeqCst);
            }
        }
        self.history.record(
            id,
            if enabled {
                Event::CommandsOn
            } else {
                Event::CommandsOff
            },
        );
        (self.on_change)();
        Ok(())
    }

    /// Ends the agent's session now; the switch stays on.
    pub fn stop(&self, id: &str) -> Result<()> {
        {
            let inner = self.inner.lock().unwrap();
            inner
                .supervisors
                .get(id)
                .ok_or_else(|| Error::UnknownEnvironment(id.into()))?
                .stop_now();
        }
        self.history.record(id, Event::Stopped);
        Ok(())
    }

    pub fn stop_all(&self) {
        let ids: Vec<String> = self
            .inner
            .lock()
            .unwrap()
            .supervisors
            .keys()
            .cloned()
            .collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    /// Forgets the Environment on this computer and deletes its key. Launchpad
    /// still lists the computer until it is removed there too.
    pub fn remove(&self, id: &str) -> Result<()> {
        let sup = {
            let mut inner = self.inner.lock().unwrap();
            let before = inner.records.len();
            inner.records.retain(|r| r.id != id);
            if inner.records.len() == before {
                return Err(Error::UnknownEnvironment(id.into()));
            }
            self.store.save(&inner.records)?;
            inner.commands.remove(id);
            inner.supervisors.remove(id)
        };
        if let Some(s) = sup {
            s.shutdown();
        }
        self.keystore.delete(&account(id))?;
        self.history.record(id, Event::Removed);
        (self.on_change)();
        Ok(())
    }

    pub fn history(&self, limit: usize) -> Vec<HistoryEntry> {
        self.history.recent(limit, None)
    }

    /// Ends every channel; used when the app quits.
    pub fn shutdown(&self) {
        let sups: Vec<Supervisor> = self
            .inner
            .lock()
            .unwrap()
            .supervisors
            .drain()
            .map(|(_, s)| s)
            .collect();
        for s in sups {
            s.shutdown();
        }
    }
}
