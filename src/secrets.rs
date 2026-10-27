//! Credential values stay in local storage, child environments and encrypted
//! daemon-to-daemon messages. Public tools expose names, never stored values.
use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::{
    bridge, config,
    server::{ApiError, AppState, DaemonMode},
};

pub const MAX_KEYS: usize = 64;
pub const MAX_VALUE_BYTES: usize = 16_384;
// JSON can escape each byte sixfold, followed by the bridge's base64 envelope.
pub const ENVIRONMENT_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;

#[cfg(test)]
mod tests;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Environment(pub BTreeMap<String, String>);

impl Drop for Environment {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

impl Environment {
    pub fn keys(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }
}

pub fn validate_key(key: &str) -> Result<(), ApiError> {
    if key.is_empty()
        || key.len() > 128
        || !key
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        || key.as_bytes()[0].is_ascii_digit()
    {
        return Err(ApiError::bad_request(
            "key must match [A-Z_][A-Z0-9_]{0,127}",
        ));
    }
    if key.starts_with("LOCALSHELLD_")
        || matches!(
            key,
            "BASH_ENV" | "ENV" | "SHELLOPTS" | "BASHOPTS" | "CDPATH"
        )
    {
        return Err(ApiError::bad_request(
            "key is reserved by the shell runtime",
        ));
    }
    Ok(())
}

pub fn normalize_keys(keys: &[String]) -> Result<Vec<String>, ApiError> {
    if keys.len() > MAX_KEYS {
        return Err(ApiError::bad_request(
            "at most 64 secret keys can be requested",
        ));
    }
    for key in keys {
        validate_key(key)?;
    }
    let mut keys = keys.to_vec();
    keys.sort();
    keys.dedup();
    Ok(keys)
}

impl AppState {
    // Paired installations use the desktop user's vault for every execution
    // identity. A standalone daemon owns its own vault.
    async fn vault_peer(&self) -> Option<crate::settings::Pairing> {
        if self.mode == DaemonMode::Privileged {
            self.settings.lock().await.saved.pairing.clone()
        } else {
            None
        }
    }

    pub(crate) async fn secret_keys(&self) -> Result<Vec<String>, ApiError> {
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::SecretsList,
                Some(Duration::from_secs(5)),
                65_536,
            )
            .await;
        }
        Ok(self
            .settings
            .lock()
            .await
            .saved
            .secrets
            .keys()
            .cloned()
            .collect())
    }

    pub(crate) async fn put_secret(&self, key: &str, value: &str) -> Result<(), ApiError> {
        validate_key(key)?;
        if value.is_empty() || value.len() > MAX_VALUE_BYTES || value.contains('\0') {
            return Err(ApiError::bad_request(
                "value must contain 1–16384 bytes without NUL",
            ));
        }
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::SecretPut {
                    key: key.into(),
                    value: value.into(),
                },
                Some(Duration::from_secs(5)),
                65_536,
            )
            .await;
        }
        let mut settings = self.settings.lock().await;
        if settings.saved.secrets.len() >= MAX_KEYS && !settings.saved.secrets.contains_key(key) {
            return Err(ApiError::bad_request(
                "at most 64 credentials can be stored",
            ));
        }
        let sealed = config::seal(value.as_bytes(), &settings.seal_key)
            .map_err(|_| ApiError::internal("credential encryption failed"))?;
        let mut next = settings.saved.clone();
        next.secrets.insert(key.into(), sealed);
        settings.save(next).await
    }

    pub(crate) async fn remove_secret(&self, key: &str) -> Result<(), ApiError> {
        validate_key(key)?;
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::SecretRemove { key: key.into() },
                Some(Duration::from_secs(5)),
                65_536,
            )
            .await;
        }
        let mut settings = self.settings.lock().await;
        let mut next = settings.saved.clone();
        next.secrets.remove(key);
        settings.save(next).await
    }

    pub(crate) async fn resolve_secrets(&self, keys: &[String]) -> Result<Environment, ApiError> {
        let keys = normalize_keys(keys)?;
        if keys.is_empty() {
            return Ok(Environment::default());
        }
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::SecretsResolve { keys },
                Some(Duration::from_secs(5)),
                ENVIRONMENT_RESPONSE_LIMIT,
            )
            .await;
        }
        let settings = self.settings.lock().await;
        let mut result = Environment::default();
        for key in keys {
            let sealed = settings
                .saved
                .secrets
                .get(&key)
                .ok_or_else(|| ApiError::bad_request(format!("unknown secret key: {key}")))?;
            let value = config::unseal(sealed, &settings.seal_key)
                .map_err(|_| ApiError::internal("credential decryption failed"))?;
            let value = std::str::from_utf8(&value)
                .map_err(|_| ApiError::internal("invalid credential encoding"))?;
            result.0.insert(key, value.into());
        }
        Ok(result)
    }
}
