//! Unit tests for the shellenv module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;

#[test]
fn inherited_hooks_are_removed_before_explicit_overrides() {
    let mut command = std::process::Command::new("node");
    command
        .env("NODE_OPTIONS", "--require inherited")
        .env("LD_PRELOAD", "inherited");
    sanitize_command(&mut command);
    let env: BTreeMap<_, _> = command.get_envs().collect();
    assert_eq!(env[std::ffi::OsStr::new("NODE_OPTIONS")], None);
    assert_eq!(env[std::ffi::OsStr::new("LD_PRELOAD")], None);
    command.env("NODE_OPTIONS", "--require explicit");
    let env: BTreeMap<_, _> = command.get_envs().collect();
    assert_eq!(
        env[std::ffi::OsStr::new("NODE_OPTIONS")],
        Some(std::ffi::OsStr::new("--require explicit"))
    );
    assert!(is_dangerous("npm_config_node_options"));
}

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

/// A login shell is free to export its own loader hooks; the supervised CLI must not receive
/// them.
///
/// `-lic` runs the interactive rc file, so anything a user (or an installer, or a compromised
/// dotfile) put there arrived in the child environment — and `NODE_OPTIONS=--require …` runs a
/// file inside the very process the window is showing. The name is reported, the value never is.
#[test]
fn drops_variables_that_would_run_code_in_the_child() {
    let output = "NODE_OPTIONS=--require /tmp/x.js\n\
                  LD_PRELOAD=/tmp/lib.so\n\
                  DYLD_INSERT_LIBRARIES=/tmp/lib.dylib\n\
                  BASH_ENV=/tmp/env.sh\n\
                  npm_config_registry=https://mirror.example\n\
                  PYTHONSTARTUP=/tmp/p.py\n\
                  DEEPSEEK_API_KEY=sk-kept\n\
                  MY_TOOL=/opt/bin\n";
    let map = parse_env(output);
    let names: Vec<&str> = map.keys().map(String::as_str).collect();
    // The two an ordinary user is entitled to export survive.
    assert_eq!(
        map.get("DEEPSEEK_API_KEY").map(String::as_str),
        Some("sk-kept")
    );
    assert_eq!(map.get("MY_TOOL").map(String::as_str), Some("/opt/bin"));
    // The hooks do not, and the answer is by name so a log line can say what was dropped.
    for key in [
        "NODE_OPTIONS",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
        "npm_config_registry",
        "PYTHONSTARTUP",
    ] {
        assert!(is_dangerous(key), "{key} 必须被判为危险键");
        assert!(!map.contains_key(key), "{key} 不能进子进程环境");
    }
    assert_eq!(names.len(), 2, "只应保留两个普通变量：{names:?}");
}

/// An ordinary identifier that merely starts with the same letters is not a loader hook.
#[test]
fn dangerous_matching_is_exact() {
    for key in [
        "NODE_OPTIONS_HELP",
        "node_options",
        "LD_PRELOADER",
        "MY_LD_PRELOAD",
    ] {
        assert!(!is_dangerous(key), "{key} 不是危险键");
    }
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
