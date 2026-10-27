//! The portable, versioned credential format shared by offline exports and sync.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zeroize::Zeroize;

pub const FORMAT: &str = "localshelld-credentials";
pub const VERSION: u32 = 1;
pub const MAX_KEYS: usize = 64;
pub const MAX_VALUE_BYTES: usize = 16_384;
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub fn validate_key(key: &str) -> Result<(), &'static str> {
    if key.is_empty()
        || key.len() > 128
        || !key
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        || key.as_bytes()[0].is_ascii_digit()
    {
        return Err("key must match [A-Z_][A-Z0-9_]{0,127}");
    }
    if key.starts_with("LOCALSHELLD_")
        || matches!(
            key,
            "BASH_ENV" | "ENV" | "SHELLOPTS" | "BASHOPTS" | "CDPATH"
        )
    {
        return Err("key is reserved by the shell runtime");
    }
    Ok(())
}

pub fn validate_credentials(credentials: &BTreeMap<String, String>) -> Result<(), &'static str> {
    if credentials.len() > MAX_KEYS {
        return Err("at most 64 credentials can be stored");
    }
    for (key, value) in credentials {
        validate_key(key)?;
        if value.is_empty() || value.len() > MAX_VALUE_BYTES || value.contains('\0') {
            return Err("value must contain 1–16384 bytes without NUL");
        }
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialFile {
    pub format: String,
    pub version: u32,
    pub credentials: BTreeMap<String, String>,
}

impl CredentialFile {
    pub fn new(credentials: BTreeMap<String, String>) -> Self {
        Self {
            format: FORMAT.into(),
            version: VERSION,
            credentials,
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != FORMAT || self.version != VERSION {
            return Err("unsupported credential file format or version");
        }
        validate_credentials(&self.credentials)
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub revision: u64,
    pub credentials: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replace {
    pub expected_revision: u64,
    pub credentials: BTreeMap<String, String>,
}

macro_rules! wipe_values {
    ($($name:ty),+) => {$ (
        impl Drop for $name {
            fn drop(&mut self) { for value in self.credentials.values_mut() { value.zeroize(); } }
        }
    )+};
}
wipe_values!(CredentialFile, Snapshot, Replace);

#[derive(Clone, Serialize, Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub refresh_expires_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_portable_credentials() {
        for key in ["", "a", "0_KEY", "LOCALSHELLD_TEST", "ENV", "BASH_ENV"] {
            assert!(validate_key(key).is_err());
        }
        for key in ["API_KEY", "_TOKEN", "SUDO_PASSWORD2"] {
            assert!(validate_key(key).is_ok());
        }
        let mut file =
            CredentialFile::new(BTreeMap::from([("TOKEN".into(), "中文\nsecret".into())]));
        assert!(file.validate().is_ok());
        file.version = 2;
        assert!(file.validate().is_err());
    }
}
