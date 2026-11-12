//! Paired Environments, their switches and channels: what the panel works with.

use crate::channel::{
    ChannelConfig, ChannelStatus, EngineSpec, KeyProvider, StatusListener, Supervisor,
};
use crate::history::{Event, History, HistoryEntry};
use crate::keys::DeviceKey;
use crate::keystore::SecretStore;
use crate::pairing::{PairingRequest, PairingSession};
use crate::store::{EnvironmentRecord, Store};
use crate::{launchpad, time, Error, Result};
use rand_core::{OsRng, RngCore};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct ManagerConfig {
    pub data_dir: PathBuf,
    pub ssh_program: PathBuf,
    pub engine: EngineSpec,
    pub computer_name: String,
    pub os: String,
    /// Extra `ssh -o` options, for tests only.
    pub extra_ssh_options: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentView {
    pub id: String,
    pub label: String,
    pub launchpad: String,
    pub device_id: String,
    pub host: String,
    pub enabled: bool,
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
}

pub struct Manager {
    cfg: ManagerConfig,
    store: Store,
    keystore: Arc<dyn SecretStore>,
    history: Arc<History>,
    on_change: ChangeListener,
    inner: Mutex<Inner>,
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
        let run_dir = cfg.data_dir.join("run");
        // Key files left behind by a crash or power loss.
        if let Ok(entries) = std::fs::read_dir(&run_dir) {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|x| x == "key") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let store = Store::new(cfg.data_dir.join("environments.json"));
        let records = store.load()?;
        let manager = Arc::new(Self {
            history: Arc::new(History::new(cfg.data_dir.join("history.jsonl"))),
            store,
            keystore,
            on_change,
            inner: Mutex::new(Inner {
                records: Vec::new(),
                supervisors: HashMap::new(),
            }),
            cfg,
        });
        {
            let mut inner = manager.inner.lock().unwrap();
            for rec in &records {
                let sup = manager.spawn_supervisor(rec);
                inner.supervisors.insert(rec.id.clone(), sup);
            }
            inner.records = records;
        }
        Ok(manager)
    }

    pub fn history_log(&self) -> &History {
        &self.history
    }

    fn spawn_supervisor(&self, rec: &EnvironmentRecord) -> Supervisor {
        let keystore = self.keystore.clone();
        let acct = account(&rec.id);
        let key: KeyProvider = Arc::new(move || {
            let seed = keystore
                .get(&acct)?
                .ok_or_else(|| Error::KeyStore("the key of this Environment is missing".into()))?;
            DeviceKey::from_seed(&seed)?.private_openssh()
        });
        let on_change = self.on_change.clone();
        let listener: StatusListener = Arc::new(move |_, _| on_change());
        let cfg = ChannelConfig {
            env_id: rec.id.clone(),
            ssh_program: self.cfg.ssh_program.clone(),
            target: rec.ssh.clone(),
            known_hosts_file: self.cfg.data_dir.join("known_hosts").join(&rec.id),
            run_dir: self.cfg.data_dir.join("run"),
            engine: self.cfg.engine.clone(),
            extra_ssh_options: self.cfg.extra_ssh_options.clone(),
        };
        Supervisor::spawn(cfg, key, self.history.clone(), listener, rec.enabled)
    }

    pub fn environments(&self) -> Vec<EnvironmentView> {
        let inner = self.inner.lock().unwrap();
        inner
            .records
            .iter()
            .map(|r| EnvironmentView {
                id: r.id.clone(),
                label: r.label.clone(),
                launchpad: r.launchpad.clone(),
                device_id: r.device_id.clone(),
                host: r.ssh.host.clone(),
                enabled: r.enabled,
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
            label: launchpad::label(&origin),
            launchpad: origin.clone(),
            device_id: result.device_id,
            ssh: result.ssh,
            enabled: true,
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
        let sup = self.spawn_supervisor(&rec);
        {
            let mut inner = self.inner.lock().unwrap();
            inner.records.push(rec.clone());
            inner.supervisors.insert(id.clone(), sup);
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
            inner.supervisors.remove(id)
        };
        if let Some(s) = sup {
            s.shutdown();
        }
        self.keystore.delete(&account(id))?;
        let _ = std::fs::remove_file(self.cfg.data_dir.join("known_hosts").join(id));
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
