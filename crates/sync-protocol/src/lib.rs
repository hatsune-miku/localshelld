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

/// Stored legacy snapshots remain readable/removable, but runtime controls must
/// never be overlaid onto a child process, including data from an older peer.
pub fn validate_injection_key(key: &str) -> Result<(), &'static str> {
    validate_key(key)?;
    if ["LD_", "DYLD_", "BASH_FUNC_", "LOCALSHELLD_"]
        .iter()
        .any(|prefix| key.starts_with(prefix))
        || matches!(
            key,
            "PATH"
                | "PATHEXT"
                | "HOME"
                | "USER"
                | "USERNAME"
                | "LOGNAME"
                | "USERPROFILE"
                | "HOMEDRIVE"
                | "HOMEPATH"
                | "APPDATA"
                | "LOCALAPPDATA"
                | "PROGRAMDATA"
                | "PROGRAMFILES"
                | "PROGRAMW6432"
                | "SYSTEMROOT"
                | "WINDIR"
                | "COMSPEC"
                | "SHELL"
                | "PWD"
                | "OLDPWD"
                | "IFS"
                | "TMP"
                | "TEMP"
                | "TMPDIR"
                | "PSMODULEPATH"
                | "PSHOME"
                | "POWERSHELL_DISTRIBUTION_CHANNEL"
                | "NODE_OPTIONS"
                | "NODE_PATH"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "PYTHONSTARTUP"
                | "PYTHONINSPECT"
                | "RUBYOPT"
                | "RUBYLIB"
                | "PERL5OPT"
                | "PERL5LIB"
                | "JAVA_TOOL_OPTIONS"
                | "JDK_JAVA_OPTIONS"
                | "_JAVA_OPTIONS"
                | "CLASSPATH"
                | "GIT_EXEC_PATH"
                | "GIT_SSH"
                | "GIT_SSH_COMMAND"
                | "GIT_ASKPASS"
                | "SSH_ASKPASS"
                | "SSH_ASKPASS_REQUIRE"
                | "SSH_AUTH_SOCK"
                | "GIT_CONFIG"
                | "GIT_CONFIG_SYSTEM"
                | "GIT_CONFIG_GLOBAL"
                | "GIT_CONFIG_COUNT"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "REQUESTS_CA_BUNDLE"
                | "CURL_CA_BUNDLE"
                | "NODE_EXTRA_CA_CERTS"
                | "NODE_TLS_REJECT_UNAUTHORIZED"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "XDG_CONFIG_HOME"
                | "XDG_DATA_HOME"
                | "XDG_RUNTIME_DIR"
                | "DBUS_SESSION_BUS_ADDRESS"
                | "DISPLAY"
                | "WAYLAND_DISPLAY"
        )
        || key.starts_with("GIT_CONFIG_KEY_")
        || key.starts_with("GIT_CONFIG_VALUE_")
        || key.starts_with("DOTNET_STARTUP_HOOKS")
        || key.starts_with("CORECLR_")
        || key.starts_with("COR_")
    {
        return Err(
            "this environment name controls the OS, shell or runtime and cannot be injected; use an application-specific credential name",
        );
    }
    Ok(())
}

pub fn validate_injection_credentials(
    credentials: &BTreeMap<String, String>,
) -> Result<(), &'static str> {
    validate_credentials(credentials)?;
    for key in credentials.keys() {
        validate_injection_key(key)?;
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
        validate_injection_credentials(&self.credentials)
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

    #[test]
    fn rejects_runtime_controls_on_every_platform_without_losing_legacy_data() {
        for key in [
            "PATH",
            "PATHEXT",
            "HOME",
            "SYSTEMROOT",
            "COMSPEC",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "NODE_OPTIONS",
            "PYTHONPATH",
            "PSMODULEPATH",
            "GIT_SSH_COMMAND",
            "GIT_CONFIG_VALUE_0",
            "HTTPS_PROXY",
            "CORECLR_ENABLE_PROFILING",
        ] {
            assert!(validate_key(key).is_ok(), "legacy {key}");
            assert!(validate_injection_key(key).is_err(), "injection {key}");
            assert!(
                CredentialFile::new(BTreeMap::from([(key.into(), "value".into())]))
                    .validate()
                    .is_err()
            );
        }
        for key in [
            "SERVICE_API_KEY",
            "SSH_PASSWORD",
            "GITHUB_TOKEN",
            "MY_APP_PATH",
        ] {
            assert!(validate_injection_key(key).is_ok(), "{key}");
        }
    }
}
