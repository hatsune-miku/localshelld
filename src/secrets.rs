//! Credential values stay in local storage, child environments and encrypted
//! daemon-to-daemon messages. Public tools expose names, never stored values.
use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::{
    bridge,
    server::{ApiError, AppState, DaemonMode},
};

pub const MAX_KEYS: usize = localshelld_sync_protocol::MAX_KEYS;
pub const MAX_VALUE_BYTES: usize = localshelld_sync_protocol::MAX_VALUE_BYTES;
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
    localshelld_sync_protocol::validate_key(key).map_err(ApiError::bad_request)
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
    pub(crate) async fn vault_peer(&self) -> Option<crate::settings::Pairing> {
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
                Some(Duration::from_secs(60)),
                65_536,
            )
            .await;
        }
        Ok(self
            .read_vault()
            .await?
            .credentials
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
                Some(Duration::from_secs(75)),
                65_536,
            )
            .await;
        }
        self.change_vault(crate::sync::Change::Put(key, value))
            .await
    }

    pub(crate) async fn remove_secret(&self, key: &str) -> Result<(), ApiError> {
        validate_key(key)?;
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::SecretRemove { key: key.into() },
                Some(Duration::from_secs(75)),
                65_536,
            )
            .await;
        }
        self.change_vault(crate::sync::Change::Remove(key)).await
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
                Some(Duration::from_secs(60)),
                ENVIRONMENT_RESPONSE_LIMIT,
            )
            .await;
        }
        let snapshot = self.read_vault().await?;
        let mut result = Environment::default();
        for key in keys {
            let value = snapshot
                .credentials
                .get(&key)
                .ok_or_else(|| ApiError::bad_request(format!("unknown secret key: {key}")))?;
            result.0.insert(key, value.clone());
        }
        Ok(result)
    }
}
