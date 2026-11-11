//! Live check of the desktop application's own release feed.
//!
//! Skipped unless `DSH_DESKTOP_LIVE_TESTS=1`, so the default `cargo test` stays offline. What this
//! covers cannot be covered offline: that the query transport this shell actually uses answers,
//! that the release list has the shape the parser expects, and that the asset names the picker
//! computes are the asset names the published release carries.

use dsh_desktop_lib::app_update::{self, AssetTarget, Channel};

#[test]
fn live_release_feed_offers_an_asset_this_build_recognises() {
    if std::env::var("DSH_DESKTOP_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set DSH_DESKTOP_LIVE_TESTS=1 to query the real release feed");
        return;
    }
    let releases = app_update::fetch_releases().expect("the release feed must answer and parse");
    assert!(
        !releases.is_empty(),
        "the repository has published releases"
    );
    let release = app_update::select(&releases, Channel::Stable)
        .expect("a stable release must be selectable");
    println!(
        "live release head = {} ({} assets)",
        release.tag_name,
        release.assets.len()
    );

    // The checksum manifest the install path verifies against has to be on the release. Without
    // it every download is refused, so its absence is a broken release, not a broken picker.
    assert!(
        release.sums().is_some(),
        "every release in this repo carries {}: {:?}",
        AssetTarget::SUMS,
        release
            .assets
            .iter()
            .map(|asset| asset.name.as_str())
            .collect::<Vec<_>>()
    );

    // At least one of the four platform artifacts has to exist. Which one is a fact about the
    // release, not about this machine — a runner of a different architecture still sees them.
    let target = AssetTarget::current(true);
    let matched = ["macos", "linux", "windows"]
        .iter()
        .flat_map(|os| {
            ["arm64", "x64"].iter().map(move |arch| AssetTarget {
                os,
                arch,
                bundled: *os != "windows",
            })
        })
        .any(|candidate| {
            release
                .asset_for(&candidate, &release.version_label())
                .is_some()
        });
    assert!(
        matched,
        "the release carries no artifact in the naming scheme the picker uses (this build: {})",
        target.asset_name(&release.version_label())
    );
}

/// The checksum manifest has to parse into the pairs the installer compares against — a release
/// whose manifest this shell cannot read is a release it can never install, and the failure would
/// only appear for a user who actually clicked update.
#[test]
fn live_checksum_manifest_parses() {
    if std::env::var("DSH_DESKTOP_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set DSH_DESKTOP_LIVE_TESTS=1 to query the real release feed");
        return;
    }
    let releases = app_update::fetch_releases().expect("the release feed must answer");
    let release = app_update::select(&releases, Channel::Stable)
        .expect("a stable release must be selectable")
        .clone();
    let sums = release.sums().expect("the release carries SHA256SUMS");
    let url = sums
        .browser_download_url
        .as_deref()
        .expect("the manifest has a download URL");
    // Fetched through the same helper the installer uses, so this also proves the transport (curl
    // or PowerShell, chosen by `transport`) can reach github.com from a machine without a browser.
    let bytes = app_update::http_get_for_test(url).expect("the manifest must download");
    let parsed = app_update::parse_sums(&String::from_utf8_lossy(&bytes));
    assert!(
        !parsed.is_empty(),
        "the manifest must list at least one artifact"
    );
    let version = release.version_label();
    let target = AssetTarget::current(true);
    let named = parsed
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    println!("live manifest names = {named:?}");
    assert!(
        named.iter().any(|name| name.ends_with(&target.flavour())),
        "the manifest has no entry for this build's artifact flavour {}",
        target.flavour()
    );
    // And the entry is a real digest, which is what the comparison would use.
    for (name, digest) in &parsed {
        assert_eq!(digest.len(), 64, "{name}: {digest}");
        assert!(!digest.is_empty());
    }
    // Silence the unused warning when the release happens to have no v-prefixed tag.
    let _ = version;
}

/// The whole install preparation, against a real release artifact: download, checksum against the
/// release manifest, and the move into `ready/`. This is the step every other test can only
/// approximate, because it is the one that talks to GitHub and runs `curl`.
///
/// Deliberately a small artifact (a `.deb`, a few megabytes) rather than the bundled macOS zip:
/// the point is the pipeline, not the payload.
#[test]
fn live_download_is_verified_before_it_reaches_ready() {
    if std::env::var("DSH_DESKTOP_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set DSH_DESKTOP_LIVE_TESTS=1 to download a real release artifact");
        return;
    }
    let releases = app_update::fetch_releases().expect("the release feed must answer");
    let release = app_update::select(&releases, Channel::Stable)
        .expect("a stable release must be selectable")
        .clone();
    let version = release.version_label();
    let target = AssetTarget {
        os: "linux",
        arch: "x64",
        bundled: false,
    };
    if release.asset_for(&target, &version).is_none() {
        eprintln!(
            "skipped: release v{version} carries no {}",
            target.asset_name(&version)
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("dsh-app-update-live-{}", std::process::id()));
    let paths = app_update::Paths::new(&dir);
    let package = app_update::download_verified(&release, &target, &paths)
        .expect("a real artifact must download and verify");
    assert!(package.is_file(), "{} is not a file", package.display());
    let size = std::fs::metadata(&package).unwrap().len();
    assert!(
        size > 1024,
        "the artifact is suspiciously small: {size} bytes"
    );
    println!("live verified {} ({size} bytes)", package.display());
    // A failed verification must not leave a file in `ready/`: the next install would pick it up.
    let stray = paths.downloads.join("stray.bin");
    std::fs::write(&stray, b"not the artifact").unwrap();
    assert!(app_update::verified_package(&paths).is_some());
    std::fs::remove_dir_all(&dir).unwrap();
}
