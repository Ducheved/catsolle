use anyhow::Result;
use getrandom::getrandom;
use rand_core::{CryptoRng, RngCore};
use ssh_key::private::{EcdsaKeypair, Ed25519Keypair, KeypairData, RsaKeypair};
use ssh_key::{EcdsaCurve, LineEnding, PrivateKey};
use std::fs;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

#[derive(Clone, Debug)]
pub enum KeyAlgorithm {
    Ed25519,
    Rsa { bits: usize },
    Ecdsa { curve: EcdsaCurve },
}

#[derive(Clone, Debug)]
pub struct GeneratedKey {
    pub private_key_path: PathBuf,
    pub public_key_path: PathBuf,
    pub fingerprint: String,
}

#[derive(Clone, Debug)]
pub struct KeyManager {
    base_dir: PathBuf,
}

impl KeyManager {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn generate(
        &self,
        name: &str,
        algorithm: KeyAlgorithm,
        passphrase: Option<Zeroizing<String>>,
        comment: Option<String>,
    ) -> Result<GeneratedKey> {
        fs::create_dir_all(&self.base_dir)?;

        let mut rng = OsRng;
        let key_data = match algorithm {
            KeyAlgorithm::Ed25519 => KeypairData::from(Ed25519Keypair::random(&mut rng)),
            KeyAlgorithm::Rsa { bits } => KeypairData::from(RsaKeypair::random(&mut rng, bits)?),
            KeyAlgorithm::Ecdsa { curve } => {
                KeypairData::from(EcdsaKeypair::random(&mut rng, curve)?)
            }
        };

        let comment = comment.unwrap_or_else(|| name.to_string());
        let key = PrivateKey::new(key_data, comment)?;
        let key = if let Some(pass) = passphrase.as_ref() {
            key.encrypt(&mut rng, pass.as_bytes())?
        } else {
            key
        };

        let private_path = self.base_dir.join(name);
        let public_path = self.base_dir.join(format!("{}.pub", name));
        let private_pem = key.to_openssh(LineEnding::LF)?;
        let public_key = key.public_key().to_openssh()?;
        fs::write(&private_path, private_pem.as_bytes())?;
        fs::write(&public_path, public_key.as_bytes())?;
        set_private_permissions(&private_path);
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

        Ok(GeneratedKey {
            private_key_path: private_path,
            public_key_path: public_path,
            fingerprint,
        })
    }

    pub fn load_private_key(&self, path: &Path, passphrase: Option<&str>) -> Result<PrivateKey> {
        let data = fs::read_to_string(path)?;
        let mut key = PrivateKey::from_openssh(&data)?;
        if key.is_encrypted() {
            let pass = passphrase.ok_or_else(|| anyhow::anyhow!("passphrase required"))?;
            key = key.decrypt(pass)?;
        }
        Ok(key)
    }
}

struct OsRng;

impl RngCore for OsRng {
    fn next_u32(&mut self) -> u32 {
        let mut buf = [0u8; 4];
        self.fill_bytes(&mut buf);
        u32::from_le_bytes(buf)
    }

    fn next_u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        self.fill_bytes(&mut buf);
        u64::from_le_bytes(buf)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        getrandom(dest).expect("getrandom failed");
    }
}

impl CryptoRng for OsRng {}

#[cfg(unix)]
fn set_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(mut perms) = fs::metadata(path).map(|m| m.permissions()) {
        perms.set_mode(0o600);
        let _ = fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) {}

use ssh_key::HashAlg;
