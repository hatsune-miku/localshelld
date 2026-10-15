//! Persisted authorization policy is separate from process-local access tokens.
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::Passkey;

use crate::{config::Config, server::ApiError, storage};

#[derive(Clone, Serialize, Deserialize)]
pub struct Pairing {
    pub key: String,
    pub peer: SocketAddr,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSettings {
    pub remembered: BTreeMap<String, bool>,
    pub pairing: Option<Pairing>,
    pub passkeys: Vec<StoredPasskey>,
    pub user_id: Option<uuid::Uuid>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredPasskey {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub key: Passkey,
}

pub struct Settings {
    pub config_path: Option<PathBuf>,
    pub config: Config,
    pub saved: SavedSettings,
}

impl Settings {
    pub fn memory(config: Config) -> Self {
        Self {
            config_path: None,
            config,
            saved: SavedSettings::default(),
        }
    }

    pub fn load(path: PathBuf, config: Config) -> anyhow::Result<Self> {
        let saved = storage::load_json(&path.with_extension("authorization.json"))?;
        Ok(Self {
            config_path: Some(path),
            config,
            saved,
        })
    }

    pub async fn save(&mut self, next: SavedSettings) -> Result<(), ApiError> {
        if let Some(path) = &self.config_path {
            let path = path.with_extension("authorization.json");
            let copy = next.clone();
            tokio::task::spawn_blocking(move || storage::save_json(&path, &copy))
                .await
                .map_err(|_| ApiError::internal("authorization storage worker failed"))?
                .map_err(|_| ApiError::internal("could not persist authorization settings"))?;
        }
        self.saved = next;
        Ok(())
    }

    pub async fn save_config(&mut self, config: Config) -> Result<(), ApiError> {
        let path = self
            .config_path
            .clone()
            .ok_or_else(|| ApiError::unavailable("configuration persistence is not configured"))?;
        let copy = config.clone();
        tokio::task::spawn_blocking(move || copy.save(&path))
            .await
            .map_err(|_| ApiError::internal("configuration worker failed"))?
            .map_err(|_| ApiError::internal("could not persist configuration"))?;
        self.config = config;
        Ok(())
    }
}
