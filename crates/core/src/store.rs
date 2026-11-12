//! Paired Environments and the state of their switches. No secrets live here.

use crate::pairing::SshTarget;
use crate::platform::write_atomic;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvironmentRecord {
    /// Local identifier, also the key store account suffix.
    pub id: String,
    pub launchpad: String,
    pub label: String,
    /// The computer's id in that Environment, assigned by Launchpad.
    pub device_id: String,
    pub ssh: SshTarget,
    /// The owner's switch.
    pub enabled: bool,
    pub paired_at: String,
    pub key_fingerprint: String,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    environments: Vec<EnvironmentRecord>,
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<Vec<EnvironmentRecord>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice::<File>(&bytes)?.environments),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, environments: &[EnvironmentRecord]) -> Result<()> {
        let file = File {
            environments: environments.to_vec(),
        };
        write_atomic(&self.path, &serde_json::to_vec_pretty(&file)?)?;
        Ok(())
    }
}
