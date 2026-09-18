//! Unit tests for the update module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;

/// A package manager that prints for its whole budget must not turn that budget into memory:
/// the drain keeps a bounded slice while still reading the pipe to the end.
#[test]
fn a_chatty_pipe_is_capped_but_still_drained() {
    // `drain` moves the reader onto its own thread, so it takes ownership of a `'static` one.
    let rx = drain(std::io::Cursor::new(vec![b'x'; 64 * 1024]), 4 * 1024);
    let captured = rx.recv_timeout(Duration::from_secs(5)).unwrap();

    assert_eq!(captured.bytes.len(), 4 * 1024, "only the cap is kept");
    assert!(captured.truncated, "an over-cap pipe must say so");
    assert!(captured.summary(5).contains("已截断"));
}

/// The reader ends when the pipe closes, so a stream below the cap is kept whole.
#[test]
fn a_short_pipe_is_kept_whole() {
    let rx = drain(std::io::Cursor::new(b"one\ntwo\nthree\n".to_vec()), 1024);
    let captured = rx.recv_timeout(Duration::from_secs(5)).unwrap();

    assert_eq!(captured.summary(2), "one | two");
    assert!(!captured.truncated);
}

/// A pipe with nothing on it is not a truncation: the message must not claim output was lost.
#[test]
fn an_empty_pipe_reports_nothing_lost() {
    let rx = drain(std::io::Cursor::new(Vec::new()), 1024);
    let captured = rx.recv_timeout(Duration::from_secs(5)).unwrap();

    assert!(captured.bytes.is_empty() && !captured.truncated);
    assert_eq!(captured.summary(5), "");
}

fn parse(raw: &str) -> Version {
    Version::parse(raw).expect("version should parse")
}

#[test]
fn orders_numeric_segments_numerically() {
    assert!(parse("1.2.10") > parse("1.2.9"));
    assert!(parse("0.1.5") > parse("0.1.4"));
}

#[test]
fn release_outranks_prerelease() {
    assert!(parse("1.0.0") > parse("1.0.0-rc.1"));
}

#[test]
fn orders_prerelease_identifiers() {
    assert!(parse("0.1.5-rc.2") > parse("0.1.5-rc.1"));
    assert!(parse("0.1.5-rc.1") > parse("0.1.5-alpha.2"));
    assert!(parse("1.0.0-alpha.1") < parse("1.0.0-alpha.beta"));
}

#[test]
fn picks_newest_among_dist_tags() {
    let tags = vec![
        ("alpha".to_string(), "0.1.5-alpha.2".to_string()),
        ("latest".to_string(), "0.1.5-rc.1".to_string()),
        ("next".to_string(), "0.1.5-rc.2".to_string()),
    ];
    let filtered: Vec<(String, String)> = tags
        .into_iter()
        .filter(|(name, _)| name == "latest" || name == "next")
        .collect();
    assert_eq!(newest_tagged(&filtered).unwrap().to_string(), "0.1.5-rc.2");
}

/// The reason `alpha` is consulted at all: upstream publishes ahead of `latest`, so a shell that
/// only reads the release tag never sees the newest build. Measured 2026-09-18, the registry
/// answered `{"latest":"0.1.5-rc.2","alpha":"0.1.6-alpha.2"}` — and `0.1.6-alpha.2` outranks
/// `0.1.5-rc.2` under semver even though its prerelease identifier sorts below `rc`.
#[test]
fn the_alpha_tag_can_be_the_newest_version_on_the_registry() {
    let tags = vec![
        ("latest".to_string(), "0.1.5-rc.2".to_string()),
        ("alpha".to_string(), "0.1.6-alpha.2".to_string()),
    ];
    // A patch release ahead of the release tag wins on the numeric segments alone.
    assert_eq!(newest_tagged(&tags).unwrap().to_string(), "0.1.6-alpha.2");
    // And it is an upgrade the shell would act on, not a downgrade.
    assert_eq!(
        judge("0.1.6-alpha.2", "0.1.5-rc.2"),
        Status::UpdateAvailable {
            from: "0.1.5-rc.2".into(),
            to: "0.1.6-alpha.2".into()
        }
    );
    // The alpha tag is not always ahead: when it is the older line, the release tag still wins.
    let behind = vec![
        ("latest".to_string(), "0.1.5-rc.2".to_string()),
        ("alpha".to_string(), "0.1.5-alpha.2".to_string()),
    ];
    assert_eq!(newest_tagged(&behind).unwrap().to_string(), "0.1.5-rc.2");
}

/// One tag list serves both the CLI and the plugin market, so a tag the registry does not publish
/// must not fail the check: `dshmarket` has no `alpha` tag at all.
#[test]
fn a_tag_the_registry_does_not_publish_is_ignored_while_another_matches() {
    // `dshmarket` today: `{"beta":…,"dev":…,"latest":"1.47.0"}` — no `alpha`.
    let market = vec![
        ("beta".to_string(), "1.19.0-beta.4".to_string()),
        ("latest".to_string(), "1.47.0".to_string()),
    ];
    let wanted = ["latest".to_string(), "alpha".to_string()];
    let selected: Vec<(String, String)> = market
        .into_iter()
        .filter(|(name, _)| wanted.iter().any(|w| w == name))
        .collect();
    assert_eq!(selected.len(), 1, "only the published tag is selected");
    assert_eq!(newest_tagged(&selected).unwrap().to_string(), "1.47.0");
}

#[test]
fn rejects_malformed_versions() {
    assert!(Version::parse("not-a-version").is_none());
    assert!(Version::parse("1.2").is_none());
    assert!(Version::parse("1.2.3.4").is_none());
}

#[test]
fn judge_reports_updates_and_up_to_date() {
    assert_eq!(
        judge("0.1.5-rc.2", "0.1.5-rc.1"),
        Status::UpdateAvailable {
            from: "0.1.5-rc.1".into(),
            to: "0.1.5-rc.2".into()
        }
    );
    assert_eq!(
        judge("0.1.5-rc.1", "0.1.5-rc.2"),
        Status::UpToDate {
            version: "0.1.5-rc.1".into()
        }
    );
    assert_eq!(
        judge("0.1.5-rc.1", "0.1.5-rc.1"),
        Status::UpToDate {
            version: "0.1.5-rc.1".into()
        }
    );
    assert!(matches!(
        judge("garbage", "0.1.5-rc.1"),
        Status::Failed { .. }
    ));
    assert!(matches!(
        judge("0.1.5-rc.1", "garbage"),
        Status::Failed { .. }
    ));
}

/// The tags a cached answer was produced from, spelled out so the assertions below read as
/// questions about a real configuration rather than about an opaque list.
fn release_tags() -> Vec<String> {
    vec!["latest".to_string()]
}

fn release_and_alpha_tags() -> Vec<String> {
    vec!["latest".to_string(), "alpha".to_string()]
}

#[test]
fn cache_freshness_follows_interval_installed_version_and_tags() {
    let entry = |latest: Option<&str>, installed: &str, tags: Vec<String>| Cache {
        checked_at: 1_000,
        installed: installed.to_string(),
        latest: latest.map(|text| text.to_string()),
        tags,
        attempted: None,
        failed: None,
        failed_at: 0,
        failures: 0,
    };
    let release = release_tags();
    // Inside the window, same installed version, same question -> fresh.
    assert!(
        entry(Some("0.1.5-rc.2"), "0.1.5-rc.1", release.clone()).is_fresh(
            1_000 + 59 * 60,
            "0.1.5-rc.1",
            &release,
            60
        )
    );
    // Past the window -> stale.
    assert!(
        !entry(Some("0.1.5-rc.2"), "0.1.5-rc.1", release.clone()).is_fresh(
            1_000 + 61 * 60,
            "0.1.5-rc.1",
            &release,
            60
        )
    );
    // Manual upgrade changed the installed version -> stale.
    assert!(
        !entry(Some("0.1.5-rc.2"), "0.1.5-rc.1", release.clone()).is_fresh(
            1_000 + 60,
            "0.1.6",
            &release,
            60
        )
    );
    // Interval 0 disables caching entirely.
    assert!(
        !entry(Some("0.1.5-rc.2"), "0.1.5-rc.1", release.clone()).is_fresh(
            1_000,
            "0.1.5-rc.1",
            &release,
            0
        )
    );
    // A failed query is retried after the short window, not the full interval.
    assert!(!entry(None, "0.1.5-rc.1", release.clone()).is_fresh(
        1_000 + 6 * 60,
        "0.1.5-rc.1",
        &release,
        360
    ));
    assert!(entry(None, "0.1.5-rc.1", release.clone()).is_fresh(
        1_000 + 4 * 60,
        "0.1.5-rc.1",
        &release,
        360
    ));
    // A different set of tags is a different question: an answer built from `latest` alone says
    // nothing about whether `alpha` has moved, so it must not be reused once the default grows.
    assert!(
        !entry(Some("0.1.5-rc.2"), "0.1.5-rc.1", release.clone()).is_fresh(
            1_000 + 60,
            "0.1.5-rc.1",
            &release_and_alpha_tags(),
            60
        )
    );
    // An entry written before the field existed carries no tags, which reads as "some other
    // question" — that is what makes a changed default take effect on the next launch.
    let legacy: Cache = serde_json::from_str(
        r#"{"checked_at":1000,"installed":"0.1.5-rc.1","latest":"0.1.5-rc.2"}"#,
    )
    .expect("a cache file from an older shell must still parse");
    assert!(legacy.tags.is_empty());
    assert!(!legacy.is_fresh(1_000 + 60, "0.1.5-rc.1", &release_and_alpha_tags(), 60));
}

#[test]
fn cache_round_trips_on_disk() {
    let dir = crate::test_dir("dsh-desktop-update-cache-test");
    let _ = std::fs::remove_dir_all(&dir);
    let cache = Cache {
        checked_at: 42,
        installed: "0.1.5-rc.1".into(),
        latest: Some("0.1.5-rc.2".into()),
        tags: release_and_alpha_tags(),
        attempted: Some("0.1.5-rc.2".into()),
        failed: None,
        failed_at: 0,
        failures: 0,
    };
    write_cache(&dir, &cache).unwrap();
    assert_eq!(read_cache(&dir), Some(cache));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cache file written before the `attempted` field existed must keep working.
#[test]
fn cache_files_without_the_attempt_field_still_parse() {
    let dir = crate::test_dir("dsh-desktop-update-cache-legacy-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        cache_path(&dir),
        r#"{"checked_at":42,"installed":"0.1.5-rc.1","latest":"0.1.5-rc.2"}"#,
    )
    .unwrap();
    let cache = read_cache(&dir).expect("legacy cache must parse");
    assert_eq!(cache.attempted, None);
    assert_eq!(cache.failed, None);
    assert_eq!(cache.failed_at, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The window expiring must not reopen an install that already proved useless.
#[test]
fn carried_attempt_follows_the_registry_answer() {
    let cache = |attempted: Option<&str>, latest: Option<&str>| Cache {
        checked_at: 42,
        installed: "0.1.5-rc.1".into(),
        latest: latest.map(str::to_string),
        tags: release_tags(),
        attempted: attempted.map(str::to_string),
        failed: None,
        failed_at: 0,
        failures: 0,
    };
    assert_eq!(
        carried_attempt(
            Some(&cache(Some("0.1.5-rc.2"), Some("0.1.5-rc.2"))),
            Some("0.1.5-rc.2")
        ),
        Some("0.1.5-rc.2".to_string())
    );
    assert_eq!(
        carried_attempt(Some(&cache(Some("0.1.5-rc.2"), None)), None),
        Some("0.1.5-rc.2".to_string())
    );
    assert_eq!(
        carried_attempt(
            Some(&cache(Some("0.1.5-rc.2"), Some("0.1.5-rc.2"))),
            Some("0.1.5-rc.3")
        ),
        None
    );
    assert_eq!(carried_attempt(None, Some("0.1.5-rc.2")), None);
    assert_eq!(
        carried_attempt(Some(&cache(None, Some("0.1.5-rc.2"))), Some("0.1.5-rc.2")),
        None
    );
}

/// End to end through `check_cached`: an expired window on a machine whose npm writes the
/// package elsewhere must not reinstall.
#[cfg(unix)]
#[test]
fn an_expired_window_does_not_reopen_an_ineffective_install() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::test_dir("dsh-desktop-update-window-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A stand-in for npm: `npm view <pkg> dist-tags --json` prints the file below.
    let tags = dir.join("dist-tags.json");
    let npm = dir.join("npm");
    std::fs::write(&npm, format!("#!/bin/sh\ncat {}\n", tags.display())).unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();

    let window_ago = now_secs() - 2 * 60 * 60;
    let seed = |latest: &str| Cache {
        checked_at: window_ago,
        installed: "0.1.5-rc.1".into(),
        latest: Some(latest.into()),
        tags: release_tags(),
        attempted: Some("0.1.5-rc.2".into()),
        failed: None,
        failed_at: 0,
        failures: 0,
    };
    let wanted = vec!["latest".to_string()];
    std::fs::write(&tags, r#"{"latest":"0.1.5-rc.2"}"#).unwrap();
    write_cache(&dir, &seed("0.1.5-rc.2")).unwrap();

    let checked = check_cached(&npm, PACKAGE, &wanted, "0.1.5-rc.1", &dir, 60);
    assert!(
        !checked.cached,
        "the window is over, so the registry is queried"
    );
    assert!(checked.attempted, "the same answer must not install again");
    assert_eq!(
        read_cache(&dir).unwrap().attempted.as_deref(),
        Some("0.1.5-rc.2"),
        "the marker survives the new query"
    );

    // A new version reopens the decision.
    std::fs::write(&tags, r#"{"latest":"0.1.5-rc.3"}"#).unwrap();
    write_cache(&dir, &seed("0.1.5-rc.2")).unwrap();
    let checked = check_cached(&npm, PACKAGE, &wanted, "0.1.5-rc.1", &dir, 60);
    assert!(!checked.attempted, "a new version is a new decision");
    assert_eq!(read_cache(&dir).unwrap().attempted, None);

    let _ = std::fs::remove_dir_all(&dir);
}
/// The repeat-install bug: npm wrote the package somewhere this shell does not run it
/// from, so the version never changes and every launch inside the cache window used to
/// stop the Harness and rebuild the tree.
#[test]
fn an_ineffective_install_is_not_retried_inside_the_window() {
    let dir = crate::test_dir("dsh-desktop-update-attempt-test");
    let _ = std::fs::remove_dir_all(&dir);
    // The cached branch never executes npm, so the path only has to exist as a value.
    let npm = Path::new("/nonexistent/npm");
    let tags = vec!["latest".to_string()];

    // What check_cached stores when it finds an update.
    write_cache(
        &dir,
        &Cache {
            checked_at: now_secs(),
            installed: "0.1.5-rc.1".into(),
            latest: Some("0.1.5-rc.2".into()),
            tags: tags.clone(),
            attempted: None,
            failed: None,
            failed_at: 0,
            failures: 0,
        },
    )
    .unwrap();
    let first = check_cached(npm, PACKAGE, &tags, "0.1.5-rc.1", &dir, 60);
    assert!(first.cached);
    assert!(
        !first.attempted,
        "a fresh answer must be allowed to install"
    );

    // The install ran and the supervised CLI did not change.
    mark_attempt_ineffective(&dir, "0.1.5-rc.2");
    let second = check_cached(npm, PACKAGE, &tags, "0.1.5-rc.1", &dir, 60);
    assert!(second.cached);
    assert!(
        second.attempted,
        "the next launch in the same window must not install again"
    );
    assert_eq!(
        second.status,
        Status::UpdateAvailable {
            from: "0.1.5-rc.1".into(),
            to: "0.1.5-rc.2".into()
        }
    );

    // A newer version than the one attempted is a new decision, not a repeat.
    write_cache(
        &dir,
        &Cache {
            checked_at: now_secs(),
            installed: "0.1.5-rc.1".into(),
            latest: Some("0.1.5-rc.3".into()),
            tags: tags.clone(),
            attempted: Some("0.1.5-rc.2".into()),
            failed: None,
            failed_at: 0,
            failures: 0,
        },
    )
    .unwrap();
    let third = check_cached(npm, PACKAGE, &tags, "0.1.5-rc.1", &dir, 60);
    assert!(!third.attempted, "only the attempted version is suppressed");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A failed install is not the same as an install that changed nothing: the first is
/// usually transient, so it must not pin the plugin (or the CLI) to an old version until
/// the registry head moves on (review A5).
#[test]
fn a_failed_install_only_suppresses_its_short_window() {
    let dir = crate::test_dir("dsh-desktop-plugin-failed-attempt-test");
    let _ = std::fs::remove_dir_all(&dir);
    // The cached branch never executes npm, so the path only has to exist as a value.
    let npm = Path::new("/nonexistent/npm");
    let tags = vec!["latest".to_string()];
    let now = now_secs();
    let cache = |failed_at: u64| Cache {
        checked_at: now,
        installed: "1.45.1".into(),
        latest: Some("1.46.1".into()),
        tags: tags.clone(),
        attempted: None,
        failed: Some("1.46.1".into()),
        failed_at,
        failures: 0,
    };

    // Just failed: this launch must report the update without stopping the Harness.
    let plugin_cache = plugin_cache_path(&dir);
    write_cache_at(&plugin_cache, &cache(now)).unwrap();
    let just_failed = check_plugin_cached(npm, MARKET_PLUGIN, &tags, "1.45.1", &dir, 60);
    assert!(just_failed.failed_recently);
    assert!(
        !just_failed.attempted,
        "a failure is not the long-lived ineffective marker"
    );

    // What the shell records when the install itself fails: the failure marker only, never
    // the long-lived "environment is wrong" one.
    mark_plugin_attempt_failed(&dir, "1.46.1");
    let stored = read_cache_at(&plugin_cache).unwrap();
    assert_eq!(stored.failed.as_deref(), Some("1.46.1"));
    assert_eq!(stored.attempted, None);
    assert!(
        stored.failed_at > 0,
        "the marker carries the time it was set"
    );

    // Past the short window the shell tries again, even though the answer is still cached.
    write_cache_at(&plugin_cache, &cache(now - FAILED_RETRY_MINUTES * 60 - 1)).unwrap();
    let later = check_plugin_cached(npm, MARKET_PLUGIN, &tags, "1.45.1", &dir, 60);
    assert!(later.cached, "the answer itself is still inside its window");
    assert!(
        !later.failed_recently,
        "one bad minute must not pin this version for ever"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_attempt_is_remembered_past_its_retry_window() {
    let failed = |at: u64, failures: u32| Cache {
        checked_at: at,
        installed: "1.45.1".into(),
        latest: Some("1.46.1".into()),
        tags: release_tags(),
        attempted: None,
        failed: Some("1.46.1".into()),
        failed_at: at,
        failures,
    };
    let marker = |at: u64, failures: u32| FailureMarker {
        version: "1.46.1".to_string(),
        at,
        failures,
    };

    // Same version, inside the window: the marker survives a fresh registry answer.
    assert_eq!(
        carried_failure(Some(&failed(1_000, 1)), Some("1.46.1"), 1_000 + 60),
        Some(marker(1_000, 1))
    );
    // Past the retry window the marker is still carried — the count has to survive the retry it
    // allowed, or a version that never boots would be retried at the shortest interval for ever.
    let past_window = 1_000 + FAILED_RETRY_MINUTES * 60;
    assert_eq!(
        carried_failure(Some(&failed(1_000, 1)), Some("1.46.1"), past_window),
        Some(marker(1_000, 1))
    );
    assert!(!failed(1_000, 1).failed_recently(
        past_window,
        &Status::UpdateAvailable {
            from: "1.45.1".into(),
            to: "1.46.1".into()
        }
    ));
    // Forgotten after a week.
    assert_eq!(
        carried_failure(
            Some(&failed(1_000, 3)),
            Some("1.46.1"),
            1_000 + FAILED_MEMORY_SECS
        ),
        None
    );
    // A newer version is a new decision, not a repeat.
    assert_eq!(
        carried_failure(Some(&failed(1_000, 1)), Some("1.46.2"), 1_000),
        None
    );
    assert_eq!(carried_failure(None, Some("1.46.1"), 1_000), None);
}

/// A version that keeps failing waits longer each time, up to a day; a different version starts
/// over. Without this a tree that can never boot was downloaded, swapped in, timed out and rolled
/// back once every five minutes.
#[test]
fn repeated_failures_of_one_version_back_off() {
    assert_eq!(failure_window_secs(0), FAILED_RETRY_MINUTES * 60);
    assert_eq!(failure_window_secs(1), FAILED_RETRY_MINUTES * 60);
    assert_eq!(failure_window_secs(2), FAILED_RETRY_MINUTES * 4 * 60);
    assert_eq!(failure_window_secs(3), FAILED_RETRY_MINUTES * 16 * 60);
    assert_eq!(failure_window_secs(20), 24 * 60 * 60);
    assert_eq!(failure_window_secs(u32::MAX), 24 * 60 * 60);

    let dir = crate::test_dir("dsh-desktop-core-failure-backoff-test");
    let _ = std::fs::remove_dir_all(&dir);
    let seeded = Cache {
        checked_at: now_secs(),
        installed: "0.1.5".into(),
        latest: Some("0.1.6".into()),
        tags: release_tags(),
        attempted: None,
        failed: None,
        failed_at: 0,
        failures: 0,
    };
    write_cache(&dir, &seeded).unwrap();
    let update = Status::UpdateAvailable {
        from: "0.1.5".into(),
        to: "0.1.6".into(),
    };

    mark_core_attempt_failed(&dir, "0.1.6");
    mark_core_attempt_failed(&dir, "0.1.6");
    let twice = read_cache(&dir).unwrap();
    assert_eq!(twice.failures, 2);
    // Ten minutes on: past the first window, still inside the second.
    assert!(twice.failed_recently(twice.failed_at + 10 * 60, &update));

    mark_core_attempt_failed(&dir, "0.1.7");
    assert_eq!(
        read_cache(&dir).unwrap().failures,
        1,
        "a new version starts over"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_market_plugin_is_read_from_the_profile_it_would_load() {
    let dir = crate::test_dir("dsh-desktop-plugin-profile-test");
    let _ = std::fs::remove_dir_all(&dir);
    let plugin = dir.join("node_modules").join(MARKET_PLUGIN);
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"dsh-profile-web","dependencies":{"dshmarket":"^1.45.1"}}"#,
    )
    .unwrap();
    std::fs::write(plugin.join("package.json"), r#"{"version":"1.46.1"}"#).unwrap();

    assert!(declares_plugin(&dir, MARKET_PLUGIN));
    // The installed version (what the CLI loads) wins over the range in the profile.
    assert_eq!(
        installed_plugin(&dir, MARKET_PLUGIN).as_deref(),
        Some("1.46.1")
    );

    std::fs::remove_file(plugin.join("package.json")).unwrap();
    assert_eq!(installed_plugin(&dir, MARKET_PLUGIN), None);

    // A plugin the user removed must not be reinstalled by the updater.
    std::fs::write(dir.join("package.json"), r#"{"dependencies":{}}"#).unwrap();
    assert!(!declares_plugin(&dir, MARKET_PLUGIN));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_plugin_check_keeps_its_own_cache_window() {
    let dir = crate::test_dir("dsh-desktop-plugin-cache-test");
    let _ = std::fs::remove_dir_all(&dir);
    assert_ne!(cache_path(&dir), plugin_cache_path(&dir));

    // The core cache holds an update for the CLI; the plugin window is separate.
    write_cache(
        &dir,
        &Cache {
            checked_at: now_secs(),
            installed: "0.1.5-rc.1".into(),
            latest: Some("0.1.5-rc.2".into()),
            tags: release_tags(),
            attempted: None,
            failed: None,
            failed_at: 0,
            failures: 0,
        },
    )
    .unwrap();
    let core_before = std::fs::read_to_string(cache_path(&dir)).unwrap();

    std::fs::write(
        plugin_cache_path(&dir),
        serde_json::to_vec(&Cache {
            checked_at: now_secs(),
            installed: "1.45.1".into(),
            latest: Some("1.46.1".into()),
            tags: release_tags(),
            attempted: None,
            failed: None,
            failed_at: 0,
            failures: 0,
        })
        .unwrap(),
    )
    .unwrap();

    // A fresh plugin answer is served from the plugin file without touching the network
    // (`npm` here does not exist) and without consuming the core answer.
    let checked = check_plugin_cached(
        Path::new("/nonexistent/npm"),
        MARKET_PLUGIN,
        &["latest".to_string()],
        "1.45.1",
        &dir,
        60,
    );
    assert!(checked.cached);
    assert_eq!(
        checked.status,
        Status::UpdateAvailable {
            from: "1.45.1".into(),
            to: "1.46.1".into()
        }
    );
    assert_eq!(
        std::fs::read_to_string(cache_path(&dir)).unwrap(),
        core_before,
        "the core cache must not be touched by a plugin check"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compatibility_flags_versions_outside_the_tested_range() {
    assert_eq!(compatibility(TESTED_MIN), Compatibility::Tested);
    assert_eq!(compatibility("0.1.5-rc.2"), Compatibility::Tested);
    assert_eq!(compatibility("0.1.9"), Compatibility::Tested);
    assert!(matches!(
        compatibility("0.1.4"),
        Compatibility::Older { .. }
    ));
    assert!(matches!(
        compatibility("0.2.0"),
        Compatibility::Newer { .. }
    ));
    assert!(matches!(
        compatibility("1.0.0"),
        Compatibility::Newer { .. }
    ));
    assert!(matches!(
        compatibility("未知"),
        Compatibility::Unknown { .. }
    ));
    // The range must stay a real range, or every version would be "Newer".
    assert!(Version::parse(TESTED_MIN).unwrap() < Version::parse(TESTED_MAX_EXCLUSIVE).unwrap());
}

/// `auto_update` and `require_tested_dsh` are on together by default, so the update path has
/// to respect the same range the startup check enforces. Installing a version that would then
/// be refused leaves the user with a CLI this shell wrote and will not run, and the working
/// version already overwritten.
#[test]
fn a_version_that_would_not_be_run_is_not_installed() {
    // Inside the tested range: install normally.
    assert!(may_install("0.1.5-rc.2", true));
    assert!(may_install("0.1.9", true));
    // The alpha line the shell now follows by default: 0.1.6-alpha.2 is inside
    // [TESTED_MIN, TESTED_MAX_EXCLUSIVE), so following the tag does not trip the gate.
    assert!(may_install("0.1.6-alpha.2", true));
    // The gate is about the tested *range*, not about prerelease status — a prerelease of the
    // boundary version sorts below it, so it is admitted, exactly like a release inside the range.
    // Recorded rather than asserted as desirable: 0.2.0-alpha.1 previews the untested 0.2.0 line,
    // and an exclusive upper bound does not exclude it.
    assert!(may_install("0.2.0-alpha.1", true));
    // Past the boundary either way, prerelease or not: refused.
    assert!(!may_install("0.2.0", true));
    assert!(!may_install("0.2.1-rc.1", true));
    // Past it: the startup check would refuse, so the install must not happen either.
    assert!(!may_install("0.2.0", true));
    assert!(!may_install("1.0.0", true));
    assert!(!may_install("未知", true));
    // With the gate switched off the user has accepted untested versions, so both steps
    // let them through.
    assert!(may_install("0.2.0", false));
    assert!(may_install("1.0.0", false));
}

#[cfg(unix)]
#[test]
fn find_pnpm_takes_the_first_executable_match_on_path() {
    use std::os::unix::fs::PermissionsExt;

    let root = crate::test_dir("dsh-desktop-find-pnpm-test");
    let empty = root.join("empty");
    let good = root.join("good");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::create_dir_all(&good).unwrap();
    let pnpm = good.join("pnpm");
    std::fs::write(&pnpm, "#!/bin/sh").unwrap();

    let path = std::env::join_paths([&empty, &good]).unwrap();
    // Readable but not executable: the CLI would die with EACCES instead of resolving it.
    assert_eq!(find_pnpm(Some(&path)), None);

    let mut mode = std::fs::metadata(&pnpm).unwrap().permissions();
    mode.set_mode(0o755);
    std::fs::set_permissions(&pnpm, mode).unwrap();
    assert_eq!(find_pnpm(Some(&path)), Some(pnpm.clone()));

    // A directory of that name is not a command either.
    let dir_like = root.join("dir-like");
    std::fs::create_dir_all(dir_like.join("pnpm")).unwrap();
    let only_a_directory = std::env::join_paths([&dir_like]).unwrap();
    assert_eq!(find_pnpm(Some(&only_a_directory)), None);

    // The still-valid match survives the checks above, and no PATH means no pnpm.
    assert_eq!(find_pnpm(Some(&path)), Some(pnpm));
    assert_eq!(find_pnpm(None), None);
    assert_eq!(find_pnpm(Some(OsStr::new(""))), None);

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn npm_path_puts_the_node_directory_first() {
    let existing = OsStr::new("/usr/bin:/opt/homebrew/bin");
    let path = npm_path(Path::new("/opt/homebrew/bin/npm"), Some(existing));
    assert_eq!(path.to_string_lossy(), "/opt/homebrew/bin:/usr/bin");
    // No PATH at all still yields the directory npm itself lives in.
    assert_eq!(
        npm_path(Path::new("/opt/homebrew/bin/npm"), None).to_string_lossy(),
        "/opt/homebrew/bin"
    );
}

#[test]
fn derives_install_prefix_from_cli_path() {
    let prefix = install_prefix(Path::new(
        "/opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js",
    ));
    assert_eq!(prefix, Some(PathBuf::from("/opt/homebrew")));
    assert_eq!(install_prefix(Path::new("/tmp/plain/bin.js")), None);
}
