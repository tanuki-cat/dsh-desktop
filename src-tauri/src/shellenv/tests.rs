//! Unit tests for the shellenv module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;

#[test]
fn parses_plain_pairs_and_ignores_noise() {
    let output = "PATH=/usr/bin:/bin\nbanner without equals\nDEEPSEEK_API_KEY=sk-abc=def\n\n1BAD=x\n=empty\n";
    let map = parse_env(output);
    assert_eq!(map.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
    assert_eq!(
        map.get("DEEPSEEK_API_KEY").map(String::as_str),
        Some("sk-abc=def")
    );
    assert!(!map.contains_key("1BAD"));
    assert_eq!(map.len(), 2);
}

#[test]
fn drops_reserved_and_app_owned_variables() {
    let output = "HOME=/Users/x\nTMPDIR=/tmp\nDSH_HOME=/Users/x/.dsh\n_=/usr/bin/env\nGOOD=1\n";
    let map = parse_env(output);
    assert_eq!(map.len(), 1);
    assert!(map.contains_key("GOOD"));
}

#[test]
fn last_duplicate_wins() {
    let map = parse_env("KEY=first\nKEY=second\n");
    assert_eq!(map.get("KEY").map(String::as_str), Some("second"));
}

#[test]
fn merges_path_with_absolute_deduplication() {
    // Entries and separator follow the platform: a Unix spelling is not absolute on
    // Windows, which is how a Windows child ended up with a POSIX-only PATH.
    let (opt, usr, local, bin, sep) = if cfg!(windows) {
        (
            r"C:\homebrew\bin",
            r"C:\usr\bin",
            r"C:\usr\local\bin",
            r"C:\bin",
            ";",
        )
    } else {
        (
            "/opt/homebrew/bin",
            "/usr/bin",
            "/usr/local/bin",
            "/bin",
            ":",
        )
    };
    let merged = merge_path(
        &[opt.to_string(), usr.to_string()],
        Some(&format!("{local}{sep}relative{sep}{usr}{sep}")),
        Some(bin),
    );
    assert_eq!(merged, format!("{opt}{sep}{usr}{sep}{local}{sep}{bin}"));
    // A relative entry never survives: in a child it would mean the child's own cwd.
    assert!(!merged.contains("relative"));
}
