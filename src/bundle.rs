//! A release is an indivisible set of role-specific programs, never one mutable exe.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

const MAX_PROGRAM: u64 = 128 * 1024 * 1024;

pub const MAINTENANCE: &str = "trustd";
pub const USER: &str = "trustd-user";
#[cfg(windows)]
pub const PRIVILEGED: &str = "trustd-system";
#[cfg(not(windows))]
pub const PRIVILEGED: &str = "trustd-root";

pub fn names(windows: bool) -> Vec<String> {
    [
        MAINTENANCE,
        USER,
        if windows {
            "trustd-system"
        } else {
            "trustd-root"
        },
        "trustd-stdio",
    ]
    .into_iter()
    .map(|name| {
        if windows {
            format!("{name}.exe")
        } else {
            name.into()
        }
    })
    .collect()
}

pub fn filename(role: &str) -> String {
    if cfg!(windows) {
        format!("{role}.exe")
    } else {
        role.into()
    }
}

pub fn beside(executable: &Path, role: &str) -> Result<PathBuf> {
    let path = executable
        .parent()
        .context("missing bundle directory")?
        .join(filename(role));
    regular(&path)?;
    Ok(path)
}

pub fn sibling(role: &str) -> Result<PathBuf> {
    beside(&std::env::current_exe()?, role)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Manifest(pub BTreeMap<String, String>);

impl Manifest {
    pub fn read(directory: &Path) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for name in names(cfg!(windows)) {
            let path = directory.join(&name);
            regular(&path)?;
            entries.insert(name, hash(&path)?);
        }
        Ok(Self(entries))
    }

    pub fn validate(&self) -> Result<()> {
        let expected = names(cfg!(windows));
        ensure!(
            self.0.len() == expected.len() && expected.iter().all(|name| self.0.contains_key(name)),
            "bundle must contain exactly the supported platform's programs"
        );
        ensure!(
            self.0
                .values()
                .all(|hash| hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit())),
            "invalid bundle digest"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }

    pub fn verify(&self, directory: &Path) -> Result<()> {
        self.validate()?;
        ensure!(
            Self::read(directory)? == *self,
            "bundle changed since verification"
        );
        Ok(())
    }

    pub fn copy(&self, source: &Path, target: &Path) -> Result<()> {
        self.validate()?;
        for (name, expected) in &self.0 {
            let from = source.join(name);
            regular(&from)?;
            let to = target.join(name);
            if !to.try_exists()? {
                // Validate the exact bytes before publishing into a readable program
                // directory. An elevated helper must not copy a raced-in source file
                // there and only discover the digest mismatch afterwards.
                let mut bytes = Zeroizing::new(Vec::new());
                fs::File::open(&from)?
                    .take(MAX_PROGRAM + 1)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    !bytes.is_empty() && bytes.len() as u64 <= MAX_PROGRAM,
                    "empty or oversized bundle program"
                );
                ensure!(
                    format!("{:x}", Sha256::digest(&*bytes)) == *expected,
                    "source bundle changed before copying: {name}"
                );
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&to)?;
                output.write_all(&bytes)?;
                output.sync_all()?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&to, fs::Permissions::from_mode(0o755))?;
                }
            }
            regular(&to)?;
            ensure!(
                hash(&to)? == *expected,
                "copied bundle program failed verification: {name}"
            );
        }
        Ok(())
    }
}

fn regular(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("missing bundle program: {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "bundle program must be a regular file"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "bundle program must not be a reparse point"
        );
    }
    Ok(())
}

pub fn hash(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    io::copy(&mut fs::File::open(path)?, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundle_requires_all_roles_and_binds_every_byte() {
        let source = tempfile::tempdir().unwrap();
        for name in names(cfg!(windows)) {
            fs::write(source.path().join(name), b"program").unwrap();
        }
        let manifest = Manifest::read(source.path()).unwrap();
        let digest = manifest.digest().unwrap();
        let target = tempfile::tempdir().unwrap();
        manifest.copy(source.path(), target.path()).unwrap();
        manifest.verify(target.path()).unwrap();
        fs::write(target.path().join(filename(USER)), b"changed").unwrap();
        assert!(manifest.verify(target.path()).is_err());
        assert!(manifest.copy(source.path(), target.path()).is_err());
        assert_ne!(
            Manifest::read(target.path()).unwrap().digest().unwrap(),
            digest
        );
        let mut hostile = manifest;
        hostile.0.insert("../outside".into(), "a".repeat(64));
        assert!(hostile.validate().is_err());
    }

    #[test]
    fn changed_source_is_not_published_even_when_copying_as_administrator() {
        let source = tempfile::tempdir().unwrap();
        for name in names(cfg!(windows)) {
            fs::write(source.path().join(name), b"approved program").unwrap();
        }
        let manifest = Manifest::read(source.path()).unwrap();
        let first = manifest.0.keys().next().unwrap();
        fs::write(source.path().join(first), b"unapproved content").unwrap();
        let target = tempfile::tempdir().unwrap();
        assert!(manifest.copy(source.path(), target.path()).is_err());
        assert!(fs::read_dir(target.path()).unwrap().next().is_none());
    }
}
