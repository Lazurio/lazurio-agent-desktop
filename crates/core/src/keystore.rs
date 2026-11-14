//! Where device key seeds live.
//!
//! macOS: Keychain (generic password). Windows: Credential Manager, whose
//! secrets Windows protects with DPAPI for the signed-in user. Both entries use
//! the service name [`crate::APP_ID`]. Other systems only get a file store for
//! development and tests.

#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::Error;
use crate::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, account: &str, secret: &[u8]) -> Result<()>;
    fn delete(&self, account: &str) -> Result<()>;
}

/// The store of the operating system, or a file store under `fallback_dir`
/// where the system has none that this build supports.
pub fn system_store(fallback_dir: PathBuf) -> Box<dyn SecretStore> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let _ = fallback_dir;
        Box::new(SystemStore::new(crate::APP_ID))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Box::new(FileStore::new(fallback_dir))
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub struct SystemStore {
    service: String,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl SystemStore {
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, account).map_err(|e| Error::KeyStore(e.to_string()))
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl SecretStore for SystemStore {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>> {
        match self.entry(account)?.get_secret() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::KeyStore(e.to_string())),
        }
    }

    fn set(&self, account: &str, secret: &[u8]) -> Result<()> {
        self.entry(account)?
            .set_secret(secret)
            .map_err(|e| Error::KeyStore(e.to_string()))
    }

    fn delete(&self, account: &str) -> Result<()> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::KeyStore(e.to_string())),
        }
    }
}

/// One file per secret with owner-only permissions. Development and tests only.
pub struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, account: &str) -> PathBuf {
        let safe: String = account
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.secret"))
    }
}

impl SecretStore for FileStore {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(account)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, account: &str, secret: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        crate::platform::write_private_file(&self.path(account), secret)?;
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        match std::fs::remove_file(self.path(account)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, Vec<u8>>>);

impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.0.lock().unwrap().get(account).cloned())
    }
    fn set(&self, account: &str, secret: &[u8]) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(account.to_string(), secret.to_vec());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("keys"));
        assert_eq!(store.get("env:a").unwrap(), None);
        store.set("env:a", b"seed").unwrap();
        assert_eq!(store.get("env:a").unwrap().as_deref(), Some(&b"seed"[..]));
        store.delete("env:a").unwrap();
        store.delete("env:a").unwrap();
        assert_eq!(store.get("env:a").unwrap(), None);
    }
}
