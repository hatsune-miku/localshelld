//! Single-writer, encrypted, crash-safe storage. No plaintext vault or JWT key
//! is ever written into the snapshot, logs or temporary files.
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fs2::FileExt;
use localshelld_sync_protocol::Snapshot;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use webauthn_rs::prelude::Passkey;
use zeroize::Zeroizing;

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredPasskey {
    pub id: String,
    pub name: String,
    pub key: Passkey,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub browser: bool,
    pub created_at: i64,
    pub expires_at: i64,
    pub refresh_hash: String,
    pub used_refresh: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Saved {
    pub user_id: uuid::Uuid,
    pub bootstrap_hash: Option<String>,
    pub jwt_key: String,
    pub passkeys: Vec<StoredPasskey>,
    pub sessions: BTreeMap<String, Session>,
    pub vault: Snapshot,
}

pub struct Store {
    pub saved: Saved,
    path: PathBuf,
    key: Zeroizing<[u8; 32]>,
    _lock: File,
}

pub fn random() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
pub fn digest(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

fn private(path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    Ok(())
}

fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    private(file.path(), false)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

impl Store {
    pub fn open(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory)?;
        private(directory, true)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("lock"))?;
        lock.try_lock_exclusive()
            .context("another sync server owns this directory")?;
        let path = directory.join("state.aes");
        let key_path = directory.join("master.key");
        let key = if key_path.exists() {
            let bytes = Zeroizing::new(fs::read(&key_path)?);
            Zeroizing::new(bytes.as_slice().try_into().context("invalid master key")?)
        } else {
            anyhow::ensure!(
                !path.exists(),
                "master key is missing; refusing to overwrite state"
            );
            let mut key = Zeroizing::new([0u8; 32]);
            OsRng.fill_bytes(key.as_mut());
            atomic(&key_path, key.as_ref())?;
            key
        };
        let cipher = Aes256Gcm::new_from_slice(key.as_ref()).expect("32 byte key");
        let saved = if path.exists() {
            let bytes = fs::read(&path)?;
            anyhow::ensure!(bytes.len() >= 12, "invalid encrypted state");
            let plain = Zeroizing::new(
                cipher
                    .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
                    .map_err(|_| anyhow::anyhow!("state decryption failed"))?,
            );
            serde_json::from_slice(&plain)?
        } else {
            let bootstrap = Zeroizing::new(random());
            atomic(&directory.join("bootstrap-code"), bootstrap.as_bytes())?;
            Saved {
                user_id: uuid::Uuid::new_v4(),
                bootstrap_hash: Some(digest(&bootstrap)),
                jwt_key: random(),
                passkeys: vec![],
                sessions: BTreeMap::new(),
                vault: Snapshot::default(),
            }
        };
        let mut store = Self {
            saved,
            path,
            key,
            _lock: lock,
        };
        store.commit(store.saved.clone())?;
        Ok(store)
    }
    pub fn commit(&mut self, next: Saved) -> Result<()> {
        let plain = Zeroizing::new(serde_json::to_vec(&next)?);
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let cipher = Aes256Gcm::new_from_slice(self.key.as_ref()).expect("32 byte key");
        let encrypted = cipher
            .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
            .map_err(|_| anyhow::anyhow!("state encryption failed"))?;
        let mut bytes = nonce.to_vec();
        bytes.extend(encrypted);
        atomic(&self.path, &bytes)?;
        self.saved = next;
        Ok(())
    }
    pub fn remove_bootstrap_file(&self) {
        // The encrypted hash is already consumed atomically; a stale file is harmless.
        let _ = fs::remove_file(self.path.with_file_name("bootstrap-code"));
    }
}
