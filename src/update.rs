//! Explicit, CLI-only updates. No service exposes an update endpoint.
pub(crate) mod service;

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "hatsune-miku/trustd";
const API: &str = "https://api.github.com/repos/hatsune-miku/trustd";
const MAX_DOWNLOAD: u64 = 128 * 1024 * 1024;
// Four role-specific programs may jointly exceed the old single-executable
// limit. Keep per-program/download limits and bound total expanded tar bytes.
const MAX_EXPANDED_BUNDLE: u64 = 4 * MAX_DOWNLOAD + 1024 * 1024;
const MAX_JSON: u64 = 4 * 1024 * 1024;

/// Reuse the same managed-registration checks for an idempotent installation.
/// This starts a stopped service but never replaces or restarts a running one.
pub(crate) fn reuse_installed_service(target: &Path, config: &Path) -> Result<bool> {
    let Some(installed) = service::discover(target)? else {
        return Ok(false);
    };
    ensure!(
        installed.config == config.canonicalize()?,
        "existing system service uses another configuration; registration was not changed"
    );
    installed.start_and_verify(None)?;
    Ok(true)
}

#[derive(Args, Debug)]
pub struct Options {
    /// Query releases without downloading, changing files, or stopping services.
    #[arg(long)]
    pub check: bool,
    /// Download and verify the complete bundle without opening the maintenance wizard.
    #[arg(long, conflicts_with = "check")]
    pub download_only: bool,
    /// Include prereleases (stable versions are eligible too).
    #[arg(long, conflicts_with = "tag")]
    pub prerelease: bool,
    /// Install this exact release tag, including a prerelease tag.
    #[arg(long)]
    pub tag: Option<String>,
    /// Permit an explicitly selected older version.
    #[arg(long, requires = "tag")]
    pub allow_downgrade: bool,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

fn version(tag: &str) -> Result<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).context("invalid semantic release version")
}

fn select_release(releases: Vec<Release>, prerelease: bool) -> Result<Release> {
    releases
        .into_iter()
        .filter_map(|release| {
            let parsed = version(&release.tag_name).ok()?;
            // GitHub Latest is a display policy; an RC tag is still prerelease
            // by semantic version, even when promoted to a full GitHub release.
            (!release.draft && (prerelease || (!release.prerelease && parsed.pre.is_empty())))
                .then_some((parsed, release))
        })
        .max_by(|a, b| a.0.cmp_precedence(&b.0))
        .map(|(_, release)| release)
        .context("no eligible release found; use --prerelease to include release candidates")
}

fn client() -> Result<Client> {
    // Keep reqwest's automatic proxy discovery: HTTPS_PROXY / ALL_PROXY and,
    // with the system-proxy feature, the current user's Windows/macOS settings.
    // Loopback daemon clients explicitly opt out with no_proxy().
    Ok(Client::builder()
        .user_agent(concat!("trustd/", env!("LOCALSHELLD_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            let permitted = matches!(
                url.host_str(),
                Some(
                    "github.com"
                        | "api.github.com"
                        | "release-assets.githubusercontent.com"
                        | "objects.githubusercontent.com"
                )
            );
            if attempt.previous().len() >= 5 || url.scheme() != "https" || !permitted {
                attempt.error("unexpected release redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "response exceeds size limit");
    Ok(bytes)
}

fn query(client: &Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .context("unable to query GitHub; check network/proxy settings")?
        .error_for_status()
        .context("GitHub query failed (public API rate limits may apply)")?;
    read_bounded(response, MAX_JSON)
}

fn find_release(client: &Client, options: &Options) -> Result<Release> {
    if let Some(tag) = &options.tag {
        version(tag)?; // Also prevents path/query injection.
        let release: Release =
            serde_json::from_slice(&query(client, &format!("{API}/releases/tags/{tag}"))?)?;
        ensure!(
            !release.draft && release.tag_name == *tag,
            "unexpected or draft release"
        );
        return Ok(release);
    }
    let mut releases = Vec::new();
    for page in 1..=20 {
        let batch: Vec<Release> = serde_json::from_slice(&query(
            client,
            &format!("{API}/releases?per_page=100&page={page}"),
        )?)?;
        let done = batch.len() < 100;
        releases.extend(batch);
        if done {
            return select_release(releases, options.prerelease);
        }
    }
    bail!("release history exceeds query limit; select an exact --tag")
}

fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("windows-x86_64"),
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("macos", "aarch64") => Ok("macos-aarch64"),
        ("macos", "x86_64") => Ok("macos-x86_64"),
        _ => bail!("no official release asset for this build target"),
    }
}

fn asset_for(release: &Release, platform: &str) -> Result<(usize, String)> {
    let extension = if platform.starts_with("windows-") {
        ".zip"
    } else {
        ".tar.gz"
    };
    let name = format!("trustd-{platform}{extension}");
    let matches: Vec<_> = release
        .assets
        .iter()
        .enumerate()
        .filter(|(_, asset)| asset.name == name)
        .collect();
    ensure!(matches.len() <= 1, "duplicate release asset {name}");
    if let Some((index, _)) = matches.first() {
        return Ok((*index, name));
    }
    bail!("release {} has no asset for {platform}", release.tag_name)
}

fn asset_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url
                .path()
                .starts_with(&format!("/{REPOSITORY}/releases/download/")),
        "untrusted release asset URL"
    );
    Ok(url)
}

fn asset_response(client: &Client, asset: &Asset) -> Result<reqwest::blocking::Response> {
    let url = asset_url(&asset.browser_download_url)?;
    Ok(client.get(url).send()?.error_for_status()?)
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn checksum_from_manifest(text: &str, name: &str) -> Result<String> {
    let mut found = None;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() == 2 && fields[1].trim_start_matches('*') == name {
            ensure!(
                found.is_none() && valid_hash(fields[0]),
                "invalid or duplicate SHA256SUMS entry"
            );
            found = Some(fields[0].to_ascii_lowercase());
        }
    }
    found.context("asset is missing from SHA256SUMS")
}

fn expected_hash(client: &Client, release: &Release, asset: &Asset) -> Result<String> {
    let digest = asset
        .digest
        .as_deref()
        .map(|value| {
            let hash = value
                .strip_prefix("sha256:")
                .context("unsupported asset digest")?;
            ensure!(valid_hash(hash), "invalid asset digest");
            Ok::<_, anyhow::Error>(hash.to_ascii_lowercase())
        })
        .transpose()?;
    let manifests: Vec<_> = release
        .assets
        .iter()
        .filter(|asset| asset.name == "SHA256SUMS")
        .collect();
    ensure!(manifests.len() <= 1, "duplicate SHA256SUMS asset");
    let manifest = manifests
        .first()
        .map(|asset_checksums| {
            let bytes = read_bounded(asset_response(client, asset_checksums)?, 64 * 1024)?;
            checksum_from_manifest(std::str::from_utf8(&bytes)?, &asset.name)
        })
        .transpose()?;
    if let (Some(digest), Some(manifest)) = (&digest, &manifest) {
        ensure!(digest == manifest, "GitHub digest and SHA256SUMS disagree");
    }
    digest
        .or(manifest)
        .context("release has no SHA-256 verification data; refusing update")
}

fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn copy_bounded(mut reader: impl Read, path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let copied = io::copy(&mut reader.by_ref().take(MAX_DOWNLOAD + 1), &mut file)?;
    ensure!(
        copied > 0 && copied <= MAX_DOWNLOAD,
        "empty or oversized release file"
    );
    file.sync_all()?;
    Ok(())
}

fn unpack(archive: &Path, name: &str, platform: &str, directory: &Path) -> Result<()> {
    ensure!(
        name.ends_with(".zip") || name.ends_with(".tar.gz"),
        "updates require a complete bundle archive"
    );
    for binary in super::bundle::names(platform.starts_with("windows-")) {
        unpack_program(
            archive,
            name,
            &format!("trustd-{platform}/{binary}"),
            &directory.join(binary),
        )?;
    }
    Ok(())
}

fn unpack_program(archive: &Path, name: &str, expected: &str, destination: &Path) -> Result<()> {
    let mut found = false;
    // Never extract arbitrary archive paths, links, permissions, or ancillary files.
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
        ensure!(zip.len() <= 100, "too many archive entries");
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index)?;
            if entry.name() != expected {
                continue;
            }
            ensure!(
                !found
                    && entry.is_file()
                    && entry
                        .unix_mode()
                        .is_none_or(|mode| mode & 0o170000 != 0o120000),
                "invalid binary archive entry"
            );
            copy_bounded(&mut entry, destination)?;
            found = true;
        }
    } else if name.ends_with(".tar.gz") {
        let decoder = flate2::read::GzDecoder::new(File::open(archive)?);
        let mut tar = tar::Archive::new(decoder.take(MAX_EXPANDED_BUNDLE + 1));
        for (index, entry) in tar.entries()?.enumerate() {
            ensure!(index < 100, "too many archive entries");
            let mut entry = entry?;
            if entry.path()?.as_ref() != Path::new(expected) {
                continue;
            }
            ensure!(
                !found && entry.header().entry_type().is_file(),
                "invalid binary archive entry"
            );
            copy_bounded(&mut entry, destination)?;
            found = true;
        }
    } else {
        copy_bounded(File::open(archive)?, destination)?;
        found = true;
    }
    ensure!(found, "archive does not contain the expected binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

pub(super) fn output(command: &mut Command) -> Result<Output> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {command:?}"))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || read_bounded(stdout, 64 * 1024));
    let err = thread::spawn(move || read_bounded(stderr, 64 * 1024));
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("command timed out: {command:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };
    Ok(Output {
        status,
        stdout: out
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader failed"))??,
        stderr: err
            .join()
            .map_err(|_| anyhow::anyhow!("stderr reader failed"))??,
    })
}

pub(super) fn checked(command: &mut Command) -> Result<String> {
    let result = output(command)?;
    ensure!(
        result.status.success(),
        "{} failed: {}",
        command.get_program().to_string_lossy(),
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(String::from_utf8(result.stdout)?)
}

fn probe(binary: &Path, role: &str) -> Result<String> {
    let text = checked(Command::new(binary).arg("--version"))?;
    let value = text
        .trim()
        .strip_prefix(&format!("{role} "))
        .context("unexpected binary version response")?;
    version(value)?;
    Ok(value.to_owned())
}

pub fn run(options: Options) -> Result<()> {
    ensure!(
        options.check || std::env::var_os("LOCALSHELLD_SESSION").is_none(),
        "updates run from an independent terminal; --check is also available inside a localshelld session"
    );
    if !options.check && !options.download_only {
        super::require_user()?;
    }
    let client = client()?;
    let release = find_release(&client, &options)?;
    let wanted = version(&release.tag_name)?;
    let current = version(localshelld::VERSION)?;
    let platform = platform()?;
    let (index, asset_name) = asset_for(&release, platform)?;
    println!(
        "Current: {} ({})\nSelected: {}\nAsset: {}",
        localshelld::VERSION,
        localshelld::COMMIT,
        release.tag_name,
        asset_name
    );
    if wanted.cmp_precedence(&current).is_eq() {
        println!("Already at the selected version.");
        return Ok(());
    }
    if wanted.cmp_precedence(&current).is_lt() && !options.allow_downgrade {
        if options.check {
            println!("No newer eligible release available.");
            return Ok(());
        }
        bail!("refusing downgrade; select --tag with --allow-downgrade explicitly");
    }
    if options.check {
        println!("Update available; no files or services were changed.");
        return Ok(());
    }
    let temp = tempfile::Builder::new()
        .prefix("trustd-update-")
        .tempdir()?;
    let archive = temp.path().join("download");
    let programs = temp.path().join("bundle");
    fs::create_dir(&programs)?;
    let asset = &release.assets[index];
    let expected = expected_hash(&client, &release, asset)?;
    println!("Downloading and verifying {}...", asset.name);
    copy_bounded(asset_response(&client, asset)?, &archive)?;
    ensure!(
        hash_file(&archive)? == expected,
        "download SHA-256 mismatch; services were not changed"
    );
    unpack(&archive, &asset_name, platform, &programs)?;
    for name in super::bundle::names(cfg!(windows)) {
        let path = programs.join(&name);
        service::validate_target(&path)?;
        ensure!(
            version(&probe(&path, name.trim_end_matches(".exe"))?)? == wanted,
            "bundle programs do not report one consistent release version"
        );
    }
    let manifest = super::bundle::Manifest::read(&programs)?;
    manifest.verify(&programs)?;
    fs::remove_file(archive)?;
    let stage = temp.keep();
    let executable = stage
        .join("bundle")
        .join(super::bundle::filename(super::bundle::MAINTENANCE));
    println!(
        "Verified bundle: {}",
        executable.parent().unwrap().display()
    );
    if options.download_only {
        println!("No services were changed. Run this bundle's trustd maintenance to apply it.");
        return Ok(());
    }
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stage.join("maintenance.log"))?;
    let mut command = localshelld::process::background_command(&executable);
    command
        .arg("maintenance")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("verified bundle is ready, but the maintenance wizard could not start")?;
    println!(
        "Maintenance wizard launched. Installation is not complete until the wizard succeeds. Log: {}",
        stage.join("maintenance.log").display()
    );
    Ok(())
}

#[cfg(test)]
mod tests;
