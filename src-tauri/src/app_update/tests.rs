//! Tests for the desktop application update path.
//!
//! Everything here is offline: the release list is the shape the GitHub API returns, the checksum
//! manifest is the shape `release.yml` produces, and the asset names are the ones the release
//! workflow actually emits. The HTTP transport itself is covered by the live tests, which only
//! run with `DSH_DESKTOP_LIVE_TESTS=1`.

use super::*;
use crate::test_dir;

fn release(tag: &str, assets: &[&str]) -> Release {
    Release {
        tag_name: tag.to_string(),
        prerelease: false,
        draft: false,
        assets: assets
            .iter()
            .map(|name| Asset {
                name: name.to_string(),
                browser_download_url: Some(format!("https://example.invalid/{name}")),
            })
            .collect(),
    }
}

fn target(os: &'static str, arch: &'static str, bundled: bool) -> AssetTarget {
    AssetTarget { os, arch, bundled }
}

fn mac_bundled() -> AssetTarget {
    target("macos", "arm64", true)
}

/// The names this picker produces have to be the ones the release workflow emits. A mismatch is a
/// release the shell can never install, and it would only surface as "no asset" on a real machine.
#[test]
fn asset_names_match_the_release_workflow() {
    let cases = [
        (
            target("macos", "arm64", true),
            "dsh-desktop_0.4.16_macos-arm64-bundled.app.zip",
        ),
        (
            target("macos", "x64", false),
            "dsh-desktop_0.4.16_macos-x64.app.zip",
        ),
        (
            target("linux", "x64", true),
            "dsh-desktop_0.4.16_linux-x64-bundled.deb",
        ),
        (
            target("linux", "arm64", false),
            "dsh-desktop_0.4.16_linux-arm64.deb",
        ),
        // Windows ships one portable package that always carries its runtime, so its name has no
        // `-bundled` suffix; `bundled` must not change it.
        (
            target("windows", "x64", true),
            "dsh-desktop_0.4.16_windows-x64-portable.zip",
        ),
        (
            target("windows", "x64", false),
            "dsh-desktop_0.4.16_windows-x64-portable.zip",
        ),
    ];
    for (target, expected) in cases {
        assert_eq!(target.asset_name("0.4.16"), expected);
        // The flavour is what the cache stores: it must be a substring of the real name, or an
        // entry would be considered stale on every single launch.
        assert!(
            expected.contains(&target.flavour()),
            "flavour {:?} is not part of {expected}",
            target.flavour()
        );
    }
}

/// The current build has to produce a name the pipeline emits, or its own updates can never be
/// found. The platform is whatever this test runs on, which is why it is written this way.
#[test]
fn the_running_build_names_an_asset_the_pipeline_produces() {
    let name = AssetTarget::current(true).asset_name("1.2.3");
    assert!(name.starts_with("dsh-desktop_1.2.3_"));
    assert!(name.contains(&AssetTarget::current(true).flavour()));
    assert!(
        name.ends_with(".app.zip") || name.ends_with(".deb") || name.ends_with(".zip"),
        "unexpected suffix in {name}"
    );
}

/// Stable skips prereleases and drafts; the prerelease channel takes them.
#[test]
fn the_channel_decides_which_release_counts() {
    let mut stable = release("v0.4.16", &[]);
    stable.prerelease = true;
    let list = vec![stable, release("v0.4.15", &[]), release("v0.4.14", &[])];
    assert_eq!(
        select(&list, Channel::Stable).map(|r| r.tag_name.as_str()),
        Some("v0.4.15")
    );
    assert_eq!(
        select(&list, Channel::Prerelease).map(|r| r.tag_name.as_str()),
        Some("v0.4.16")
    );

    // A draft is never served, whatever the channel: the release is not published yet.
    let mut draft = release("v0.4.17", &[]);
    draft.draft = true;
    let list = vec![draft, release("v0.4.15", &[])];
    assert_eq!(
        select(&list, Channel::Prerelease).map(|r| r.tag_name.as_str()),
        Some("v0.4.15")
    );
}

/// A tag this shell cannot parse must not be selected: it would sort unpredictably and then fail
/// to produce an asset name.
#[test]
fn unparseable_tags_are_skipped_rather_than_blamed() {
    let list = vec![release("nightly", &[]), release("v0.4.15", &[])];
    assert_eq!(
        select(&list, Channel::Stable).map(|r| r.tag_name.as_str()),
        Some("v0.4.15")
    );
    assert!(select(&[release("nightly", &[])], Channel::Stable).is_none());
}

/// The comparison decides whether the asset is even looked for: a release older than what runs
/// must not produce "no asset for this platform" instead of "up to date".
#[test]
fn a_release_against_the_running_version_is_judged_before_its_assets() {
    let current = "0.4.16";
    let older = release("v0.4.15", &[]);
    assert_eq!(
        judge(current, &older, &mac_bundled()),
        Status::UpToDate {
            version: "0.4.15".to_string()
        }
    );
    let same = release("v0.4.16", &[]);
    assert!(matches!(
        judge(current, &same, &mac_bundled()),
        Status::UpToDate { .. }
    ));

    let newer = release(
        "v0.4.17",
        &["dsh-desktop_0.4.17_macos-arm64-bundled.app.zip"],
    );
    assert_eq!(
        judge(current, &newer, &mac_bundled()),
        Status::UpdateAvailable {
            from: "0.4.16".to_string(),
            to: "0.4.17".to_string(),
        }
    );
}

/// An asset for another architecture is not a substitute: installing it would be worse than
/// saying the release has nothing for this machine.
#[test]
fn a_release_without_this_platforms_asset_is_reported_not_substituted() {
    let release = release("v0.4.17", &["dsh-desktop_0.4.17_macos-x64-bundled.app.zip"]);
    assert_eq!(
        judge("0.4.16", &release, &mac_bundled()),
        Status::NoAsset {
            to: "0.4.17".to_string(),
            wanted: "dsh-desktop_0.4.17_macos-arm64-bundled.app.zip".to_string(),
        }
    );
    // The thin build is a different artifact as well: it has no bundled runtime to update.
    let thin = target("macos", "arm64", false);
    assert_eq!(
        judge("0.4.16", &release, &thin),
        Status::NoAsset {
            to: "0.4.17".to_string(),
            wanted: "dsh-desktop_0.4.17_macos-arm64.app.zip".to_string(),
        }
    );
}

/// The manifest the pipeline writes, including the blank line and the `*` prefix some `sha256sum`
/// invocations emit. A line that cannot be parsed is dropped, not fatal.
#[test]
fn the_checksum_manifest_is_parsed_the_way_the_pipeline_writes_it() {
    let digest = "a".repeat(64);
    let text = format!(
        "{digest}  dsh-desktop_0.4.16_macos-arm64.app.zip\n\n{digest} *dsh-desktop_0.4.16_linux-x64.deb\nnot-a-digest  ignored.zip\n"
    );
    let sums = parse_sums(&text);
    assert_eq!(sums.len(), 2);
    assert_eq!(
        checksum_for(&sums, "dsh-desktop_0.4.16_macos-arm64.app.zip"),
        Some(digest.as_str())
    );
    assert_eq!(
        checksum_for(&sums, "dsh-desktop_0.4.16_linux-x64.deb"),
        Some(digest.as_str()),
        "the binary-mode marker must be stripped"
    );
    assert_eq!(checksum_for(&sums, "ignored.zip"), None);
    assert_eq!(checksum_for(&sums, "not-in-the-file"), None);
}

/// A checksum that does not match must leave nothing behind: a wrong file kept in `ready/` is a
/// wrong file the next install can pick up. The deletion is the check's own job, so this asserts
/// the file is gone without doing it here.
#[test]
fn a_package_whose_checksum_does_not_match_is_deleted() {
    let dir = test_dir("app-update-checksum");
    let paths = Paths::new(&dir);
    paths.create().unwrap();
    let partial = paths
        .downloads
        .join("dsh-desktop_0.4.16_macos-arm64-bundled.app.zip");
    std::fs::write(&partial, b"not the package").unwrap();
    let actual = sha256_file(&partial).unwrap();
    assert_eq!(actual.len(), 64);

    let error = check_digest(&partial, &"0".repeat(64)).unwrap_err();
    assert!(
        error.contains(&actual),
        "the actual digest must be named: {error}"
    );
    assert!(!partial.exists(), "a mismatched package must be removed");

    // The matching case keeps the file and hands the digest back.
    std::fs::write(&partial, b"not the package").unwrap();
    assert_eq!(check_digest(&partial, &actual).unwrap(), actual);
    assert!(partial.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A package already verified in `ready/` is the same answer a download would produce, so it is
/// reused instead of fetching hundreds of megabytes again. One that no longer matches would be
/// installed by accident, so it is dropped.
#[test]
fn a_verified_package_in_ready_is_reused_and_a_stale_one_is_dropped() {
    let dir = test_dir("app-update-reuse");
    let paths = Paths::new(&dir);
    paths.create().unwrap();
    let name = "dsh-desktop_0.4.16_macos-arm64-bundled.app.zip";
    let stored = paths.ready.join(name);
    std::fs::write(&stored, b"the package").unwrap();
    let digest = sha256_file(&stored).unwrap();

    assert_eq!(reuse_verified(&paths, name, &digest), Some(stored.clone()));
    assert!(stored.is_file(), "a matching package must be kept");

    // The release now describes different bytes: the old copy is not that release.
    assert_eq!(reuse_verified(&paths, name, &"0".repeat(64)), None);
    assert!(!stored.exists(), "a stale package must be removed");
    // Nothing there at all is the ordinary first download.
    assert_eq!(reuse_verified(&paths, name, &digest), None);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The version is read back out of the package's own file name: a package left in `ready/` by an
/// earlier session carries nothing else, and the `.app` inside has to report exactly this.
#[test]
fn the_version_is_read_from_the_asset_name() {
    for (name, expected) in [
        (
            "dsh-desktop_0.4.15_macos-arm64-bundled.app.zip",
            Some("0.4.15"),
        ),
        ("dsh-desktop_0.4.15_linux-x64.deb", Some("0.4.15")),
        (
            "dsh-desktop_0.4.15_windows-x64-portable.zip",
            Some("0.4.15"),
        ),
        (
            "dsh-desktop_0.5.0-beta.1_macos-x64.app.zip",
            Some("0.5.0-beta.1"),
        ),
        ("SHA256SUMS", None),
        ("dsh-desktop_.app.zip", None),
        ("dsh-desktop_not-a-version_macos-x64.app.zip", None),
        ("something-else_0.4.15.app.zip", None),
    ] {
        assert_eq!(
            version_of_asset_name(name).as_deref(),
            expected,
            "version parsed from {name}"
        );
    }
}

/// The relaunch has to release the single-instance socket before this process goes away. A
/// replacement that finds a listener connects to it, is answered "already running", and exits —
/// and this one is leaving at the same moment, which would leave no instance at all.
///
/// Source-level because the ordering is the whole property and every step needs a running app:
/// there is no seam here that would not exist only for this test.
#[test]
fn the_relaunch_releases_the_singleton_before_the_process_leaves() {
    let source = include_str!("../app_update.rs");
    let start = source
        .find("pub fn relaunch_into_update(")
        .expect("relaunch_into_update exists");
    let body = &source[start..];
    let end = body.find("\n}\n").expect("the function ends");
    let body = &body[..end];
    let released = body
        .find("tauri_plugin_single_instance::destroy(app)")
        .expect("the socket is released explicitly, not only on RunEvent::Exit");
    let launched = body
        .find("macos::relaunch(")
        .expect("the new copy is started from here");
    assert!(
        launched < released,
        "the release must follow the launch: {body}"
    );
    assert!(
        !body.contains("sleep("),
        "waiting for the replacement to claim the lock cannot work: it never does"
    );
}

/// A package waiting in `ready/` may already be behind the running application. Installing it
/// would be a downgrade, so the version is compared before anything is unpacked.
#[cfg(target_os = "macos")]
#[test]
fn a_waiting_package_older_than_the_running_app_is_refused() {
    // `0.0.0` is older than any real version and `99.0.0` is newer, so neither case depends on
    // the version in the manifest.
    let older = refuse_downgrade("0.0.0").unwrap_err();
    assert!(older.contains("拒绝降级"), "{older}");
    assert!(
        refuse_downgrade(env!("CARGO_PKG_VERSION")).is_err(),
        "re-installing the running version is not an update either"
    );
    assert!(refuse_downgrade("99.0.0").is_ok());
    // A name this shell cannot read is left to `macos::verify_bundle`, which checks the bundle
    // itself: refusing here would turn a naming change into "cannot install".
    assert!(refuse_downgrade("not-a-version").is_ok());
}

/// Two files with different contents hash differently, and the digest is lowercase hex of the
/// right length — the format `SHA256SUMS` is compared against.
#[test]
fn the_digest_is_lowercase_hex_of_the_right_length() {
    let dir = test_dir("app-update-digest");
    std::fs::create_dir_all(&dir).unwrap();
    let first = dir.join("first.bin");
    let second = dir.join("second.bin");
    std::fs::write(&first, b"one").unwrap();
    std::fs::write(&second, b"two").unwrap();
    let a = sha256_file(&first).unwrap();
    let b = sha256_file(&second).unwrap();
    assert_eq!(a.len(), 64);
    assert!(a
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(a, b);
    // The empty-input digest is a well-known constant, which pins the hash itself rather than
    // only its shape.
    let empty = dir.join("empty.bin");
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(
        sha256_file(&empty).unwrap(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The cache answers one question only: a different channel, flavour or installed version is a
/// different question.
#[test]
fn a_cached_answer_is_only_reused_for_the_question_that_produced_it() {
    let now = 1_000_000;
    let cache = Cache {
        checked_at: now,
        installed: "0.4.16".to_string(),
        latest: Some("0.4.17".to_string()),
        channel: "stable".to_string(),
        flavour: mac_bundled().flavour(),
        failed: None,
        failed_at: 0,
        failures: 0,
    };
    assert!(cache.is_fresh(now, "0.4.16", Channel::Stable, &mac_bundled(), 60));
    // A different installed version, channel or build flavour invalidates it.
    assert!(!cache.is_fresh(now, "0.4.15", Channel::Stable, &mac_bundled(), 60));
    assert!(!cache.is_fresh(now, "0.4.16", Channel::Prerelease, &mac_bundled(), 60));
    assert!(!cache.is_fresh(
        now,
        "0.4.16",
        Channel::Stable,
        &target("macos", "arm64", false),
        60
    ));
    // The interval is a real deadline, and 0 disables reuse entirely.
    assert!(!cache.is_fresh(now + 60 * 60, "0.4.16", Channel::Stable, &mac_bundled(), 60));
    assert!(!cache.is_fresh(now, "0.4.16", Channel::Stable, &mac_bundled(), 0));
}

/// A failed query is retried on the short window, not trusted for the whole interval: a dead
/// network at launch is exactly the case where the next launch should ask again soon.
#[test]
fn a_failed_query_is_retried_before_the_interval_ends() {
    let now = 1_000_000;
    let cache = Cache {
        checked_at: now,
        installed: "0.4.16".to_string(),
        latest: None,
        channel: "stable".to_string(),
        flavour: mac_bundled().flavour(),
        failed: None,
        failed_at: 0,
        failures: 0,
    };
    assert!(cache.is_fresh(now, "0.4.16", Channel::Stable, &mac_bundled(), 60));
    assert!(!cache.is_fresh(
        now + crate::update::FAILED_RETRY_MINUTES * 60,
        "0.4.16",
        Channel::Stable,
        &mac_bundled(),
        60
    ));
}

/// The retry window grows with repeated failures of one version, and forgets a different one.
#[test]
fn repeated_failures_of_one_version_back_off() {
    let dir = test_dir("app-update-failures");
    std::fs::create_dir_all(&dir).unwrap();
    mark_failed(&dir, "0.4.17");
    let first = read_cache(&dir).unwrap();
    assert_eq!(first.failures, 1);
    assert_eq!(first.failed.as_deref(), Some("0.4.17"));
    mark_failed(&dir, "0.4.17");
    let second = read_cache(&dir).unwrap();
    assert_eq!(second.failures, 2);
    assert!(
        failure_window_secs(second.failures) > failure_window_secs(first.failures),
        "the wait has to grow with each repeat"
    );
    mark_failed(&dir, "0.4.18");
    let third = read_cache(&dir).unwrap();
    assert_eq!(third.failures, 1, "a new version starts the count over");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Policy and channel names are the ones `config.json` documents; anything else is rejected so the
/// caller can report it rather than silently reading a typo as the default.
#[test]
fn policy_and_channel_names_round_trip() {
    for (text, policy) in [
        ("off", Policy::Off),
        ("notify", Policy::Notify),
        ("download", Policy::Download),
        ("install", Policy::Install),
    ] {
        assert_eq!(Policy::parse(text), Some(policy));
        assert_eq!(Policy::parse(&text.to_uppercase()), Some(policy));
        assert_eq!(policy.label(), text);
    }
    assert_eq!(Policy::parse("always"), None);
    assert_eq!(Policy::default(), Policy::Notify);
    assert_eq!(Channel::parse("stable"), Some(Channel::Stable));
    assert_eq!(Channel::parse("prerelease"), Some(Channel::Prerelease));
    assert_eq!(Channel::parse("beta"), None);
    assert_eq!(Channel::default(), Channel::Stable);
}

/// An empty release list is not "up to date": `select` answers `None` and the caller reports it,
/// which is what keeps a GitHub error document from reading as a clean bill of health.
#[test]
fn an_empty_list_selects_nothing() {
    assert!(select(&[], Channel::Stable).is_none());
    assert!(select(&[], Channel::Prerelease).is_none());
}

/// Only macOS replaces the running installation; everywhere else the update stops at a verified
/// download and says so.
#[test]
fn in_place_installation_is_macos_only() {
    assert_eq!(install_supported(), cfg!(target_os = "macos"));
}

/// The hint has to name the exact step for the platform, because it is the only thing the user is
/// given after a download this shell will not install.
///
/// macOS used to fall into the Linux branch and print `sudo apt install` on a machine that has no
/// apt — the one sentence a user reaches for when the package is not installed for them.
#[test]
fn the_manual_install_hint_names_the_package() {
    let hint = crate::app_update::manual_install_hint(Path::new("/tmp/pkg"));
    assert!(hint.contains("/tmp/pkg"));
    if cfg!(target_os = "windows") {
        assert!(hint.contains("解压"), "{hint}");
    } else if cfg!(target_os = "macos") {
        assert!(hint.contains("DSH Desktop.app"), "{hint}");
        assert!(!hint.contains("apt"), "macOS has no apt: {hint}");
    } else {
        assert!(hint.contains("apt install"), "{hint}");
    }
}

/// The backup is named after the version it *holds*, not the one being installed: a rollback has
/// to find the version it would go back to, and `0.4.16.app` containing 0.4.15 is a trap.
#[cfg(target_os = "macos")]
#[test]
fn the_backup_is_named_after_the_version_it_replaces() {
    let dir = test_dir("app-update-backup-name");
    let bundle = dir.join("DSH Desktop.app");
    std::fs::create_dir_all(bundle.join("Contents")).unwrap();
    let write = |version: &str| {
        let plist = format!(
            "<plist><dict>\n<key>CFBundleShortVersionString</key>\n<string>{version}</string>\n\
             <key>CFBundleIdentifier</key>\n<string>{BUNDLE_ID}</string>\n</dict></plist>\n"
        );
        std::fs::write(bundle.join("Contents").join("Info.plist"), plist).unwrap();
    };
    let backups = dir.join("backup");
    write("0.4.15");
    assert_eq!(
        macos::backup_path(&backups, &bundle, "0.4.16"),
        backups.join("0.4.15.app")
    );
    // A bundle whose plist cannot be read still has to produce a path: falling back to the
    // version being installed is wrong by one version, while refusing to install is worse.
    std::fs::remove_file(bundle.join("Contents").join("Info.plist")).unwrap();
    assert_eq!(
        macos::backup_path(&backups, &bundle, "0.4.16"),
        backups.join("0.4.16.app")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(target_os = "macos")]
/// Tauri writes this shape, and the installer reads exactly these two keys before replacing
/// anything: an app whose identifier or version does not match is not installed.
#[test]
fn the_bundle_plist_is_read_the_way_tauri_writes_it() {
    // Trimmed from a real build's `Contents/Info.plist`: the keys are interleaved with other
    // entries, which is what the sequential split has to survive.
    let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleDisplayName</key>
    <string>DSH Desktop</string>
    <key>CFBundleExecutable</key>
    <string>dsh-desktop</string>
    <key>CFBundleIdentifier</key>
    <string>com.deepseek.dsh.desktop</string>
    <key>CFBundleShortVersionString</key>
    <string>0.4.15</string>
    <key>CFBundleVersion</key>
    <string>0.4.15</string>
</dict>
</plist>"#;
    assert_eq!(
        macos::plist_string(plist, "CFBundleIdentifier").as_deref(),
        Some(BUNDLE_ID)
    );
    assert_eq!(
        macos::plist_string(plist, "CFBundleShortVersionString").as_deref(),
        Some("0.4.15")
    );
    // `CFBundleVersion` must not be confused with `CFBundleShortVersionString`: the search is
    // anchored on the exact key, not on a prefix.
    assert_eq!(
        macos::plist_string(plist, "CFBundleVersion").as_deref(),
        Some("0.4.15")
    );
    assert_eq!(macos::plist_string(plist, "CFBundleNonsense"), None);
}

/// A package with the wrong identifier or version is refused before anything is moved.
#[cfg(target_os = "macos")]
#[test]
fn a_bundle_that_is_not_this_application_is_refused() {
    let dir = test_dir("app-update-bundle-verify");
    let bundle = dir.join("Fake.app");
    std::fs::create_dir_all(bundle.join("Contents")).unwrap();
    let write = |identifier: &str, version: &str| {
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n \
             <key>CFBundleIdentifier</key>\n    <string>{identifier}</string>\n \
             <key>CFBundleShortVersionString</key>\n    <string>{version}</string>\n</dict>\n</plist>\n"
        );
        std::fs::write(bundle.join("Contents").join("Info.plist"), plist).unwrap();
    };
    write("com.example.other", "0.4.16");
    let error = macos::verify_bundle(&bundle, "0.4.16").unwrap_err();
    assert!(error.contains("CFBundleIdentifier"), "{error}");
    write(BUNDLE_ID, "0.4.15");
    let error = macos::verify_bundle(&bundle, "0.4.16").unwrap_err();
    assert!(error.contains("版本"), "{error}");
    write(BUNDLE_ID, "0.4.16");
    assert!(macos::verify_bundle(&bundle, "0.4.16").is_ok());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(target_os = "macos")]
/// The bundle root is found by walking up from the executable, because that is the only path the
/// process knows about itself: `Contents/MacOS/dsh-desktop` → `Contents` → `DSH Desktop.app`.
#[test]
fn the_bundle_root_is_found_from_the_executable_path() {
    let executable = Path::new("/Applications/DSH Desktop.app/Contents/MacOS/dsh-desktop");
    assert_eq!(
        macos::bundle_root(executable).as_deref(),
        Some(Path::new("/Applications/DSH Desktop.app"))
    );
    // A binary run straight out of `target/release` is not a bundle, and the installer has to say
    // so rather than renaming a directory that only looks like one.
    assert_eq!(
        macos::bundle_root(Path::new("/repo/src-tauri/target/release/dsh-desktop")),
        None
    );
    // A directory that merely mentions `.app` is not a bundle either: the check is on the
    // extension of an ancestor, not on a substring of the whole path.
    assert_eq!(
        macos::bundle_root(Path::new("/tmp/app.bundle/Contents/MacOS/x")),
        None
    );
}
