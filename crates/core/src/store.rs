//! Paired Environments and the state of their switches. No secrets live here.

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
    /// `wss://<launchpad host>/.lazurio/computers/attach`. Empty in records
    /// written by v0 (SSH); [`Store::load`] derives it from the Launchpad.
    #[serde(default)]
    pub channel_url: String,
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

    /// The records, with v0 records moved to the HTTPS channel. Returns
    /// whether something was migrated (the caller saves then).
    pub fn load(&self) -> Result<(Vec<EnvironmentRecord>, bool)> {
        let mut records = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<File>(&bytes)?.environments,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let mut migrated = false;
        for rec in &mut records {
            if rec.channel_url.is_empty() {
                if let Some(url) = crate::wire::derive_channel_url(&rec.launchpad) {
                    rec.channel_url = url;
                    migrated = true;
                }
            }
        }
        Ok((records, migrated))
    }

    pub fn save(&self, environments: &[EnvironmentRecord]) -> Result<()> {
        let file = File {
            environments: environments.to_vec(),
        };
        write_atomic(&self.path, &serde_json::to_vec_pretty(&file)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_v0_records_to_the_https_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("environments.json");
        std::fs::write(
            &path,
            r#"{"environments":[{"id":"a1","launchpad":"https://launchpad.acme.example.com","label":"acme","device_id":"office-mac","ssh":{"host":"h","user":"u","port":22,"knownHosts":["h ssh-ed25519 AAAA"]},"enabled":true,"paired_at":"2026-11-12","key_fingerprint":"SHA256:x"}]}"#,
        )
        .unwrap();
        let store = Store::new(path.clone());
        let (records, migrated) = store.load().unwrap();
        assert!(migrated);
        assert_eq!(
            records[0].channel_url,
            "wss://launchpad.acme.example.com/.lazurio/computers/attach"
        );
        store.save(&records).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("knownHosts"));
        assert!(!store.load().unwrap().1);
    }
}
