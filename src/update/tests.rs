use super::*;
use std::io::Write;

#[test]
fn updater_keeps_system_proxy_support_with_default_features_disabled() {
    // reqwest's environment-variable discovery remains available even when OS
    // proxy discovery is accidentally disabled, so env-proxy tests alone would
    // not catch this regression. Guard the application's feature contract too.
    let manifest: toml::Value = toml::from_str(include_str!("../../Cargo.toml")).unwrap();
    let features = manifest["dependencies"]["reqwest"]["features"]
        .as_array()
        .unwrap();
    assert!(
        features
            .iter()
            .any(|feature| feature.as_str() == Some("system-proxy"))
    );
}

fn release(tag: &str, prerelease: bool, draft: bool) -> Release {
    Release {
        tag_name: tag.into(),
        prerelease,
        draft,
        assets: vec![],
    }
}

#[test]
fn selects_by_semver_not_api_order_or_promoted_rc() {
    let versions = || {
        vec![
            release("v0.1.0-rc.10.1", false, false),
            release("v0.1.0-rc.9.1", true, false),
            release("v0.0.9", false, false),
            release("v9.0.0", false, true),
        ]
    };
    assert_eq!(
        select_release(versions(), false).unwrap().tag_name,
        "v0.0.9"
    );
    assert_eq!(
        select_release(versions(), true).unwrap().tag_name,
        "v0.1.0-rc.10.1"
    );
    assert!(select_release(vec![release("v0.1.0-rc.8.1", false, false)], false).is_err());
    assert!(version("../latest?x=y").is_err());
}

#[test]
fn chooses_only_the_requested_platform_asset() {
    let mut release = release("v1.0.0", false, false);
    for name in [
        "trustd-windows-x86_64.exe",
        "trustd-windows-x86_64.zip",
        "trustd-linux-x86_64.tar.gz",
    ] {
        release.assets.push(Asset {
            name: name.into(),
            browser_download_url: String::new(),
            digest: None,
        });
    }
    assert_eq!(
        asset_for(&release, "windows-x86_64").unwrap().1,
        "trustd-windows-x86_64.zip"
    );
    assert!(asset_for(&release, "macos-aarch64").is_err());
}

#[test]
fn checksum_requires_exact_unique_filename_and_valid_digest() {
    let hash = "ab".repeat(32);
    assert_eq!(
        checksum_from_manifest(&format!("{hash}  target.zip\n"), "target.zip").unwrap(),
        hash
    );
    assert_eq!(
        checksum_from_manifest(&format!("{hash} *target.zip\n"), "target.zip").unwrap(),
        hash
    );
    assert!(checksum_from_manifest(&format!("{hash}  evil-target.zip"), "target.zip").is_err());
    assert!(checksum_from_manifest("abc  target.zip", "target.zip").is_err());
    assert!(
        checksum_from_manifest(
            &format!("{hash} target.zip\n{hash} target.zip"),
            "target.zip"
        )
        .is_err()
    );
}

#[test]
fn bounded_reads_reject_oversized_data() {
    assert_eq!(read_bounded(&b"abc"[..], 3).unwrap(), b"abc");
    assert!(read_bounded(&b"abcd"[..], 3).is_err());
}

#[test]
fn release_source_matches_the_renamed_repository_without_widening_trust() {
    assert_eq!(API, format!("https://api.github.com/repos/{REPOSITORY}"));
    for name in [
        "trustd-windows-x86_64.zip",
        "trustd-macos-aarch64.tar.gz",
        "SHA256SUMS",
    ] {
        let url = format!(
            "https://github.com/hatsune-miku/trustd/releases/download/v0.1.0-rc.35.1/{name}"
        );
        assert_eq!(asset_url(&url).unwrap().as_str(), url);
    }
    for url in [
        "https://github.com/hatsune-miku/localshelld/releases/download/v1.0.0/tool.zip",
        "https://github.com/another-owner/trustd/releases/download/v1.0.0/tool.zip",
        "https://github.com/hatsune-miku/trustd-other/releases/download/v1.0.0/tool.zip",
        "https://github.com.example.org/hatsune-miku/trustd/releases/download/v1.0.0/tool.zip",
        "http://github.com/hatsune-miku/trustd/releases/download/v1.0.0/tool.zip",
        "https://user@github.com/hatsune-miku/trustd/releases/download/v1.0.0/tool.zip",
        "https://github.com:8443/hatsune-miku/trustd/releases/download/v1.0.0/tool.zip",
    ] {
        assert!(asset_url(url).is_err(), "unexpected trusted URL: {url}");
    }
}

#[test]
fn rejects_external_asset_url_before_network_access() {
    let asset = Asset {
        name: "evil".into(),
        browser_download_url: "https://example.org/tool.exe".into(),
        digest: None,
    };
    assert!(asset_response(&client().unwrap(), &asset).is_err());
}

#[test]
fn zip_extracts_only_exact_binary_not_path_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("download");
    let mut archive = zip::ZipWriter::new(File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default();
    archive.start_file("../outside", options).unwrap();
    archive.write_all(b"bad").unwrap();
    archive
        .start_file("trustd-windows-x86_64/trustd.exe", options)
        .unwrap();
    archive.write_all(b"binary").unwrap();
    for binary in ["trustd-user.exe", "trustd-system.exe", "trustd-stdio.exe"] {
        archive
            .start_file(format!("trustd-windows-x86_64/{binary}"), options)
            .unwrap();
        archive.write_all(b"role").unwrap();
    }
    archive.finish().unwrap();
    let destination = temp.path().join("new");
    fs::create_dir(&destination).unwrap();
    unpack(&path, "asset.zip", "windows-x86_64", &destination).unwrap();
    assert_eq!(fs::read(destination.join("trustd.exe")).unwrap(), b"binary");
    assert!(!temp.path().parent().unwrap().join("outside").exists());
}

#[test]
fn tar_expansion_limit_allows_multiple_programs_larger_than_one_program() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bundle.tar.gz");
    let gzip =
        flate2::write::GzEncoder::new(File::create(&path).unwrap(), flate2::Compression::fast());
    let mut archive = tar::Builder::new(gzip);
    let mut header = tar::Header::new_gnu();
    header.set_size(MAX_DOWNLOAD);
    header.set_mode(0o755);
    header.set_cksum();
    archive
        .append_data(
            &mut header,
            "trustd-linux-x86_64/trustd-root",
            io::repeat(0).take(MAX_DOWNLOAD),
        )
        .unwrap();
    header.set_size(7);
    header.set_cksum();
    archive
        .append_data(
            &mut header,
            "trustd-linux-x86_64/trustd-stdio",
            &b"program"[..],
        )
        .unwrap();
    archive.into_inner().unwrap().finish().unwrap();
    let destination = temp.path().join("stdio");
    unpack_program(
        &path,
        "bundle.tar.gz",
        "trustd-linux-x86_64/trustd-stdio",
        &destination,
    )
    .unwrap();
    assert_eq!(fs::read(destination).unwrap(), b"program");
}

#[test]
fn tar_rejects_symbolic_link_binary() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("download");
    let gzip =
        flate2::write::GzEncoder::new(File::create(&path).unwrap(), flate2::Compression::default());
    let mut archive = tar::Builder::new(gzip);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    archive
        .append_link(&mut header, "trustd-linux-x86_64/trustd", "/etc/passwd")
        .unwrap();
    archive.into_inner().unwrap().finish().unwrap();
    assert!(
        unpack(
            &path,
            "asset.tar.gz",
            "linux-x86_64",
            &temp.path().join("new")
        )
        .is_err()
    );
}

#[test]
fn incomplete_duplicate_and_raw_release_bundles_are_rejected() {
    for duplicate in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("download");
        let mut archive = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        archive
            .start_file("trustd-windows-x86_64/trustd.exe", options)
            .unwrap();
        archive.write_all(b"program").unwrap();
        if duplicate {
            // ZipWriter rejects duplicate paths too, so use tar for the duplicate fixture.
            archive.finish().unwrap();
            let gzip = flate2::write::GzEncoder::new(
                File::create(&path).unwrap(),
                flate2::Compression::default(),
            );
            let mut tar = tar::Builder::new(gzip);
            for _ in 0..2 {
                let mut header = tar::Header::new_gnu();
                header.set_size(7);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, "trustd-linux-x86_64/trustd", &b"program"[..])
                    .unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        } else {
            archive.finish().unwrap();
        }
        let target = temp.path().join("bundle");
        fs::create_dir(&target).unwrap();
        let (name, platform) = if duplicate {
            ("asset.tar.gz", "linux-x86_64")
        } else {
            ("asset.zip", "windows-x86_64")
        };
        assert!(unpack(&path, name, platform, &target).is_err());
        assert!(unpack(&path, "single.exe", "windows-x86_64", &target).is_err());
    }
}

#[test]
fn update_target_requires_existing_absolute_regular_file() {
    let temp = tempfile::tempdir().unwrap();
    assert!(service::validate_target(Path::new("relative.exe")).is_err());
    assert!(service::validate_target(temp.path()).is_err());
    assert!(service::validate_target(&temp.path().join("missing.exe")).is_err());
}
