//! The device key: one ed25519 key per paired Environment.
//!
//! Only the 32-byte seed is stored (in the system key store). The OpenSSH
//! private key file exists just long enough for `ssh` to authenticate.

use crate::{Error, Result};
use rand_core::OsRng;
use ssh_key::private::{Ed25519Keypair, KeypairData};
use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey, PublicKey};
use zeroize::Zeroizing;

pub struct DeviceKey(PrivateKey);

impl DeviceKey {
    pub fn generate() -> Result<Self> {
        PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
            .map(Self)
            .map_err(|e| Error::Key(e.to_string()))
    }

    pub fn from_seed(seed: &[u8]) -> Result<Self> {
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| Error::Key("stored key has the wrong length".into()))?;
        let pair = Ed25519Keypair::from_seed(&seed);
        PrivateKey::new(KeypairData::Ed25519(pair), "")
            .map(Self)
            .map_err(|e| Error::Key(e.to_string()))
    }

    pub fn seed(&self) -> Zeroizing<Vec<u8>> {
        let pair = self
            .0
            .key_data()
            .ed25519()
            .expect("device keys are always ed25519");
        Zeroizing::new(pair.private.to_bytes().to_vec())
    }

    /// `ssh-ed25519 AAAA…` without a comment; Launchpad adds its own.
    pub fn public_openssh(&self) -> String {
        PublicKey::new(self.0.public_key().key_data().clone(), "")
            .to_openssh()
            .expect("an ed25519 public key always encodes")
    }

    /// `SHA256:…`, the same fingerprint Launchpad shows on its approval page.
    pub fn fingerprint(&self) -> String {
        self.0.public_key().fingerprint(HashAlg::Sha256).to_string()
    }

    /// The unencrypted OpenSSH private key file contents.
    pub fn private_openssh(&self) -> Result<Zeroizing<String>> {
        self.0
            .to_openssh(LineEnding::LF)
            .map_err(|e| Error::Key(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_round_trip_keeps_the_public_key() {
        let key = DeviceKey::generate().unwrap();
        let again = DeviceKey::from_seed(&key.seed()).unwrap();
        assert_eq!(key.public_openssh(), again.public_openssh());
        assert!(key.public_openssh().starts_with("ssh-ed25519 "));
        assert_eq!(key.public_openssh().split(' ').count(), 2);
        assert!(key.fingerprint().starts_with("SHA256:"));
        let pem = again.private_openssh().unwrap();
        assert!(pem.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    }

    #[test]
    fn rejects_a_short_seed() {
        assert!(DeviceKey::from_seed(&[0u8; 7]).is_err());
    }
}
