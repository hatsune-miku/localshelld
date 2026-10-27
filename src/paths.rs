//! Standard machine installation layout; user state deliberately lives elsewhere.
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize)]
pub struct MachinePaths {
    pub program: PathBuf,
    pub data: PathBuf,
    pub config: PathBuf,
}

impl MachinePaths {
    pub fn resolve() -> Result<Self> {
        #[cfg(windows)]
        let (program, data) = (
            localshelld_native_process::program_files_directory()?.join("localshelld"),
            localshelld_native_process::program_data_directory()?.join("localshelld"),
        );
        #[cfg(target_os = "macos")]
        let (program, data) = (
            PathBuf::from("/Library/localshelld"),
            PathBuf::from("/Library/Application Support/localshelld"),
        );
        #[cfg(target_os = "linux")]
        let (program, data) = (
            PathBuf::from("/opt/localshelld"),
            PathBuf::from("/etc/localshelld"),
        );
        let config = data.join("system/config.toml");
        Ok(Self {
            program,
            data,
            config,
        })
    }

    pub fn executable(&self, hash: &str) -> Result<PathBuf> {
        anyhow::ensure!(
            hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid executable digest"
        );
        Ok(self
            .program
            .join("versions")
            .join(hash)
            .join(if cfg!(windows) {
                "localshelld.exe"
            } else {
                "localshelld"
            }))
    }

    pub fn contains_system_state(&self, path: &Path) -> bool {
        normalized(path).starts_with(&(normalized(&self.data) + "/"))
            || normalized(path) == normalized(&self.data)
    }
}

fn normalized(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text
        .strip_prefix("//?/")
        .unwrap_or(&text)
        .trim_end_matches('/');
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_paths_cannot_escape_the_program_directory() {
        let paths = MachinePaths::resolve().unwrap();
        assert!(
            paths
                .executable(&"a".repeat(64))
                .unwrap()
                .starts_with(&paths.program)
        );
        for value in ["", "../outside", "a/b", "not-a-hash"] {
            assert!(paths.executable(value).is_err());
        }
        assert!(paths.contains_system_state(&paths.config));
        assert!(!paths.contains_system_state(&PathBuf::from(format!(
            "{}-other/config.toml",
            paths.data.display()
        ))));
    }
}
