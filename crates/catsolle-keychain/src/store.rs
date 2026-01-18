use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::Result;
use argon2::Argon2;
use keyring::Entry;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("keyring error: {0}")]
    Keyring(String),
    #[error("encryption error: {0}")]
    Crypto(String),
    #[error("not found")]
    NotFound,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SecretRef {
    pub id: String,
}

#[derive(Clone, Debug)]
pub struct KeychainManager {
    service: String,
    fallback_file: PathBuf,
    fallback_enabled: bool,
}

impl KeychainManager {
    pub fn new(service: impl Into<String>, fallback_file: PathBuf, fallback_enabled: bool) -> Self {
        Self {
            service: service.into(),
            fallback_file,
            fallback_enabled,
        }
    }

    pub fn store_secret(
        &self,
        id: &str,
        secret: &Zeroizing<String>,
        master: Option<&str>,
    ) -> Result<(), SecretError> {
        if let Ok(entry) = Entry::new(&self.service, id) {
            if entry.set_password(secret).is_ok() {
                return Ok(());
            }
        }
        if self.fallback_enabled {
            let master = master
                .ok_or_else(|| SecretError::Crypto("master password required".to_string()))?;
            let mut map = self.read_fallback(master).unwrap_or_default();
            map.insert(id.to_string(), secret.to_string());
            self.write_fallback(master, &map)?;
            Ok(())
        } else {
            Err(SecretError::Keyring("store failed".to_string()))
        }
    }

    pub fn get_secret(
        &self,
        id: &str,
        master: Option<&str>,
    ) -> Result<Option<Zeroizing<String>>, SecretError> {
        if let Ok(entry) = Entry::new(&self.service, id) {
            if let Ok(value) = entry.get_password() {
                return Ok(Some(Zeroizing::new(value)));
            }
        }
        if self.fallback_enabled {
            let master = master
                .ok_or_else(|| SecretError::Crypto("master password required".to_string()))?;
            let map = self.read_fallback(master).unwrap_or_default();
            Ok(map.get(id).map(|v| Zeroizing::new(v.clone())))
        } else {
            Ok(None)
        }
    }

    pub fn delete_secret(&self, id: &str, master: Option<&str>) -> Result<(), SecretError> {
        if let Ok(entry) = Entry::new(&self.service, id) {
            let _ = entry.delete_password();
        }
        if self.fallback_enabled {
            let master = master
                .ok_or_else(|| SecretError::Crypto("master password required".to_string()))?;
            let mut map = self.read_fallback(master).unwrap_or_default();
            map.remove(id);
            self.write_fallback(master, &map)?;
        }
        Ok(())
    }

    fn read_fallback(&self, master: &str) -> Result<HashMap<String, String>, SecretError> {
        if !self.fallback_file.exists() {
            return Ok(HashMap::new());
        }
        let data = fs::read(&self.fallback_file).map_err(|e| SecretError::Crypto(e.to_string()))?;
        let plain = decrypt(master, &data)?;
        let map: HashMap<String, String> =
            serde_json::from_slice(&plain).map_err(|e| SecretError::Crypto(e.to_string()))?;
        Ok(map)
    }

    fn write_fallback(
        &self,
        master: &str,
        map: &HashMap<String, String>,
    ) -> Result<(), SecretError> {
        if let Some(parent) = self.fallback_file.parent() {
            fs::create_dir_all(parent).map_err(|e| SecretError::Crypto(e.to_string()))?;
        }
        let plain = serde_json::to_vec(map).map_err(|e| SecretError::Crypto(e.to_string()))?;
        let data = encrypt(master, &plain)?;
        fs::write(&self.fallback_file, data).map_err(|e| SecretError::Crypto(e.to_string()))?;
        Ok(())
    }
}

const MAGIC: &[u8; 6] = b"CATSK1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

fn encrypt(master: &str, plain: &[u8]) -> Result<Vec<u8>, SecretError> {
    let mut salt = [0u8; SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    let key = derive_key(master, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| SecretError::Crypto(e.to_string()))?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plain)
        .map_err(|e| SecretError::Crypto(e.to_string()))?;
    let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn decrypt(master: &str, data: &[u8]) -> Result<Vec<u8>, SecretError> {
    if data.len() < MAGIC.len() + SALT_LEN + NONCE_LEN || &data[..MAGIC.len()] != MAGIC {
        return Err(SecretError::Crypto("invalid secret store".to_string()));
    }
    let salt = &data[MAGIC.len()..MAGIC.len() + SALT_LEN];
    let nonce_start = MAGIC.len() + SALT_LEN;
    let nonce = &data[nonce_start..nonce_start + NONCE_LEN];
    let ct = &data[nonce_start + NONCE_LEN..];
    let key = derive_key(master, salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| SecretError::Crypto(e.to_string()))?;
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|e| SecretError::Crypto(e.to_string()))
}

fn derive_key(master: &str, salt: &[u8]) -> Result<[u8; 32], SecretError> {
    let mut key = [0u8; 32];
    let argon = Argon2::default();
    argon
        .hash_password_into(master.as_bytes(), salt, &mut key)
        .map_err(|e| SecretError::Crypto(e.to_string()))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fallback_roundtrip() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("secrets.enc");
        let manager = KeychainManager::new("catsolle", file.clone(), true);
        let secret = Zeroizing::new("secret".to_string());
        manager.store_secret("id", &secret, Some("master")).unwrap();
        let loaded = manager.get_secret("id", Some("master")).unwrap().unwrap();
        assert_eq!(&*loaded, "secret");
        manager.delete_secret("id", Some("master")).unwrap();
        let missing = manager.get_secret("id", Some("master")).unwrap();
        assert!(missing.is_none());
    }
}
