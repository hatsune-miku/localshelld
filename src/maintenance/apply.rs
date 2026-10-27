use super::{Operation, Plan, digest_file, platform};
use anyhow::{Context, Result, ensure};
use localshelld::{
    config::{self, Config},
    paths::MachinePaths,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use zeroize::Zeroizing;

pub(crate) fn apply(path: &Path, digest: &str) -> Result<()> {
    ensure!(
        super::super::is_elevated()?,
        "maintenance helper requires Administrator/root"
    );
    let plan = read_plan(path, digest)?;
    let paths = MachinePaths::resolve()?;
    let result = apply_plan(&plan, &paths);
    if let Err(error) = &result {
        // Never write a privileged result into a caller-controlled directory.
        if prepare_directory(&paths.data, false).is_ok() {
            let _ = private_write(
                &paths.data.join("maintenance-error.txt"),
                format!("{error:#}\n").as_bytes(),
            );
        }
    }
    result
}

fn read_plan(path: &Path, digest: &str) -> Result<Plan> {
    ensure!(
        digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid plan digest"
    );
    regular(path)?;
    let mut bytes = Zeroizing::new(Vec::new());
    fs::File::open(path)?.take(32769).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 32768 && format!("{:x}", Sha256::digest(&*bytes)) == digest,
        "installation plan changed or exceeds the size limit"
    );
    let plan: Plan = serde_json::from_slice(&bytes)?;
    ensure!(plan.protocol == 1, "unsupported installation plan");
    ensure!(
        plan.system_bind.ip().is_loopback() && plan.system_bind.port() != 0,
        "invalid system endpoint"
    );
    ensure!(
        digest_file(&std::env::current_exe()?)? == plan.binary_hash,
        "installer binary does not match the approved plan"
    );
    #[cfg(windows)]
    {
        ensure!(
            plan.owner == platform::owner()?,
            "approve UAC using the SAME logged-in account, not another administrator"
        );
        ensure!(
            plan.user_config == config::user_config_path()?,
            "unexpected user configuration path"
        );
        super::super::user_service::require_interactive_owner()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(path)?;
        let uid: u32 = plan.owner.parse().context("invalid login owner")?;
        ensure!(
            uid != 0 && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
            "plan must be private to the invoking non-root user"
        );
    }
    if let Some(hash) = &plan.password_hash {
        let parsed = argon2::PasswordHash::new(hash)
            .map_err(|_| anyhow::anyhow!("invalid password verifier"))?;
        ensure!(
            parsed.algorithm.as_str() == "argon2id",
            "invalid password verifier algorithm"
        );
    }
    Ok(plan)
}

fn apply_plan(plan: &Plan, paths: &MachinePaths) -> Result<()> {
    let previous = super::super::update::service::registration()?;
    if plan.operation == Operation::Uninstall {
        #[cfg(windows)]
        super::super::user_service::uninstall()?;
        return super::super::uninstall_system();
    }
    let shell = platform::shell()?;
    ensure!(
        shell.is_file(),
        "system-level shell is missing: {}",
        shell.display()
    );
    let source = previous
        .as_ref()
        .map(|value| value.config.clone())
        .filter(|value| !same_file(value, &paths.config));
    if let Some(source) = &source {
        regular(source)?;
        ensure!(
            !paths.config.try_exists()?,
            "both an old service configuration and a standard configuration exist; reconcile them before migration"
        );
    }
    let existing_config = source.as_deref().unwrap_or(&paths.config);
    if existing_config.try_exists()? {
        let config = Config::load(existing_config)?;
        ensure!(
            config.bind == plan.system_bind,
            "system and user endpoints differ; existing configuration was not overwritten"
        );
    } else {
        ensure!(
            plan.password_hash.is_some(),
            "a Master Password is required for first-time initialization"
        );
    }
    prepare_directory(&paths.program, true)?;
    prepare_directory(&paths.data, false)?;
    let system_directory = paths
        .config
        .parent()
        .context("missing system configuration directory")?;
    prepare_directory(system_directory, false)?;
    let versions = paths.program.join("versions");
    prepare_directory(&versions, true)?;
    let executable = paths.executable(&plan.binary_hash)?;
    prepare_directory(executable.parent().unwrap(), true)?;
    if executable.try_exists()? {
        regular(&executable)?;
        ensure!(
            digest_file(&executable)? == plan.binary_hash,
            "existing version directory contains a different executable"
        );
    } else {
        // create_new + protected parent avoids replacing an attacker-controlled
        // symlink/hardlink or an executable in use by the maintenance UI.
        let mut source = fs::File::open(std::env::current_exe()?)?;
        let mut target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&executable)?;
        std::io::copy(&mut source, &mut target)?;
        target.sync_all()?;
    }
    protect(&executable, false, true)?;
    ensure!(
        digest_file(&executable)? == plan.binary_hash,
        "installed executable digest mismatch"
    );

    let backups = paths.data.join("backups");
    prepare_directory(&backups, false)?;
    let backup = backups.join(uuid::Uuid::new_v4().simple().to_string());
    prepare_directory(&backup, false)?;
    private_write(
        &backup.join("registration.json"),
        &serde_json::to_vec_pretty(&previous)?,
    )?;
    // Stop before copying mutable authorization state; source files are never
    // removed. Rollback can re-register the old executable/configuration.
    if previous.is_some() {
        super::super::uninstall_system()?;
    }
    let result = (|| -> Result<()> {
        if existing_config.try_exists()? {
            backup_configuration(existing_config, &backup)?;
        }
        if let Some(source) = &source {
            migrate_configuration(source, &paths.config)?;
            // Keeping the legacy copy for recovery must not leave the same
            // system credentials readable by the non-elevated account.
            for path in state_files(source) {
                if path.try_exists()? {
                    regular(&path)?;
                    protect(&path, false, false)?;
                }
            }
        }
        let mut config = if paths.config.try_exists()? {
            Config::load(&paths.config)?
        } else {
            Config {
                password_hash: plan
                    .password_hash
                    .clone()
                    .context("missing initial password verifier")?,
                bind: plan.system_bind,
                ..Config::default()
            }
        };
        // The SYSTEM/root service never searches the installing user's PATH.
        config.shell = shell.to_string_lossy().into_owned();
        config.save(&paths.config)?;
        config::load_or_create_seal_key(&config::seal_key_path(&paths.config))?;
        for path in state_files(&paths.config) {
            if path.try_exists()? {
                regular(&path)?;
                protect(&path, false, false)?;
            }
        }
        verify_preserved_state(&backup, &paths.config)?;
        super::super::install_system_at(&executable, &paths.config)?;
        super::super::installation::wait_healthy(
            config.bind,
            "privileged",
            Duration::from_secs(30),
        )?;
        Ok(())
    })();
    if let Err(error) = result {
        let recovery = (|| -> Result<()> {
            super::super::uninstall_system()?;
            if source.is_none() && backup.join("config.toml").is_file() {
                restore_configuration(&backup, &paths.config)?;
            }
            if source.is_some() {
                // These exact files did not exist before this migration. The
                // unchanged originals and protected backup remain recoverable.
                for path in state_files(&paths.config) {
                    if path.try_exists()? {
                        fs::remove_file(path)?;
                    }
                }
            }
            if let Some(previous) = &previous {
                super::super::install_system_at(&previous.executable, &previous.config)?;
            }
            Ok(())
        })();
        return Err(error).with_context(|| match recovery {
            Ok(()) => format!("system installation failed; previous registration restored where available; backup: {}", backup.display()),
            Err(recovery) => format!("system installation and rollback failed ({recovery:#}); backup: {}", backup.display()),
        });
    }
    #[cfg(windows)]
    {
        // Same-account elevated management can replace an administrator-owned
        // old task, but the new task still uses InteractiveToken/LeastPrivilege.
        super::super::user_service::uninstall()?;
        super::super::user_service::install_at(&executable, &plan.user_config)
            .context("system service is ready, but user autostart failed; rerun repair")?;
    }
    // Public receipt contains only paths and successful checks, never secret
    // values or password verifiers. The UI can inspect it without elevation.
    let receipt = paths.program.join("installation.json");
    private_write(
        &receipt,
        &serde_json::to_vec_pretty(&serde_json::json!({
            "version": localshelld::VERSION,
            "binary_sha256": plan.binary_hash,
            "system_config": paths.config,
            "backup": backup,
            "previous_state_verified": backup.join("config.toml").is_file(),
            "legacy_files_restricted": source.is_some(),
        }))?,
    )?;
    protect(&receipt, false, true)?;
    Ok(())
}

fn verify_preserved_state(backup: &Path, target: &Path) -> Result<()> {
    let previous = backup.join("config.toml");
    if previous.is_file() {
        let mut before = Config::load(&previous)?;
        let after = Config::load(target)?;
        before.shell = after.shell.clone();
        ensure!(
            serde_json::to_value(before)? == serde_json::to_value(after)?,
            "configuration changed beyond the system shell path"
        );
    }
    for (name, target) in [
        (
            "config.authorization.json",
            target.with_extension("authorization.json"),
        ),
        ("seal.key", config::seal_key_path(target)),
    ] {
        let previous = backup.join(name);
        if previous.is_file() {
            ensure!(
                digest_file(&previous)? == digest_file(&target)?,
                "authorization state/key changed during migration"
            );
        }
    }
    Ok(())
}

fn state_files(config: &Path) -> [PathBuf; 3] {
    [
        config.to_owned(),
        config.with_extension("authorization.json"),
        config::seal_key_path(config),
    ]
}

fn same_file(left: &Path, right: &Path) -> bool {
    left == right || matches!((left.canonicalize(), right.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

fn backup_configuration(config: &Path, backup: &Path) -> Result<()> {
    for (source, name) in
        state_files(config)
            .iter()
            .zip(["config.toml", "config.authorization.json", "seal.key"])
    {
        if source.try_exists()? {
            private_write(&backup.join(name), &read_state(source)?)?;
        }
    }
    Ok(())
}
fn restore_configuration(backup: &Path, config: &Path) -> Result<()> {
    for (target, name) in
        state_files(config)
            .iter()
            .zip(["config.toml", "config.authorization.json", "seal.key"])
    {
        let source = backup.join(name);
        if source.try_exists()? {
            private_write(target, &read_state(&source)?)?;
        }
    }
    Ok(())
}
fn migrate_configuration(source: &Path, target: &Path) -> Result<()> {
    let values = state_files(source)
        .iter()
        .map(|path| {
            if path.try_exists()? {
                read_state(path).map(Some)
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    for path in state_files(target) {
        ensure!(
            !path.try_exists()?,
            "migration target already exists: {}",
            path.display()
        );
    }
    for (target, value) in state_files(target).iter().zip(&values) {
        if let Some(value) = value {
            private_write(target, value)?;
        }
    }
    Ok(())
}
fn read_state(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    regular(path)?;
    let mut bytes = Zeroizing::new(Vec::new());
    fs::File::open(path)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "configuration file is too large"
    );
    Ok(bytes)
}
fn regular(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !linked(&metadata),
        "expected a regular non-linked file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.nlink() == 1,
            "hard-linked state files are not supported"
        );
    }
    Ok(())
}
fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
fn prepare_directory(path: &Path, program: bool) -> Result<()> {
    // Only callers' fixed application directories are passed here. Do not
    // recursively change permissions on Program Files, ProgramData or /Library.
    if !path.try_exists()? {
        fs::create_dir(path).with_context(|| format!("could not create {}", path.display()))?;
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !linked(&metadata),
        "installation directory must not be a symlink/junction: {}",
        path.display()
    );
    protect(path, true, program)
}
fn protect(path: &Path, directory: bool, program: bool) -> Result<()> {
    #[cfg(windows)]
    {
        use localshelld_native_process::{FileAccess, restrict_path};
        restrict_path(
            path,
            directory,
            if program {
                FileAccess::Program
            } else {
                FileAccess::System
            },
        )?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            fs::symlink_metadata(path)?.uid() == 0,
            "machine installation path is not root-owned: {}",
            path.display()
        );
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if program {
                0o755
            } else if directory {
                0o700
            } else {
                0o600
            }),
        )?;
    }
    Ok(())
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.try_exists()? {
        regular(path)?;
    }
    let directory = path.parent().context("missing parent")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    protect(file.path(), false, false)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_verification_allows_only_shell_normalization() {
        let directory = tempfile::tempdir().unwrap();
        let backup = directory.path().join("backup");
        let target = directory.path().join("target");
        fs::create_dir(&backup).unwrap();
        fs::create_dir(&target).unwrap();
        let config = Config {
            password_hash: "test-verifier".into(),
            ..Config::default()
        };
        fs::write(
            backup.join("config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        let mut normalized = config.clone();
        normalized.shell = platform::shell().unwrap().to_string_lossy().into_owned();
        let path = target.join("config.toml");
        fs::write(&path, toml::to_string(&normalized).unwrap()).unwrap();
        for folder in [&backup, &target] {
            fs::write(folder.join("seal.key"), b"test-key").unwrap();
            fs::write(folder.join("config.authorization.json"), b"test-state").unwrap();
        }
        assert!(verify_preserved_state(&backup, &path).is_ok());
        fs::write(target.join("seal.key"), b"changed-key").unwrap();
        assert!(verify_preserved_state(&backup, &path).is_err());
        fs::write(target.join("seal.key"), b"test-key").unwrap();
        normalized.password_hash = "changed-verifier".into();
        fs::write(&path, toml::to_string(&normalized).unwrap()).unwrap();
        assert!(verify_preserved_state(&backup, &path).is_err());
    }
    #[test]
    fn only_the_three_owned_state_files_are_migrated() {
        let files = state_files(Path::new("/old/example.toml"));
        assert_eq!(
            files,
            [
                PathBuf::from("/old/example.toml"),
                PathBuf::from("/old/example.authorization.json"),
                PathBuf::from("/old/seal.key")
            ]
        );
    }
    #[test]
    fn changed_or_oversized_plans_are_rejected_before_any_system_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.json");
        fs::write(&path, b"{}").unwrap();
        assert!(read_plan(&path, &"0".repeat(64)).is_err());
        assert!(read_plan(&path, "../bad").is_err());
        fs::write(&path, vec![b' '; 32769]).unwrap();
        assert!(read_plan(&path, &"0".repeat(64)).is_err());
    }
}
