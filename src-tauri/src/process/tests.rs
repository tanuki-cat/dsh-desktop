//! Unit tests for the process module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;

#[cfg(unix)]
#[test]
fn terminate_stops_a_spawned_process_group() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    // Same shape as the supervised Harness: own process group, silent stdio.
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    assert!(is_alive(pid));

    let started = Instant::now();
    assert!(
        terminate(pid, Duration::from_secs(3)),
        "terminate must report success"
    );
    assert!(!is_alive(pid), "pid {pid} must be gone after terminate");
    // A child that exited but was never reaped used to look alive for the whole grace
    // period, so quit always took the full timeout. SIGTERM must settle this fast.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "terminate waited for a zombie: {:?}",
        started.elapsed()
    );
    let _ = child.wait();
}

#[test]
fn state_round_trip() {
    let dir = std::env::temp_dir().join("dsh-desktop-state-test");
    let _ = std::fs::remove_dir_all(&dir);
    let state = HarnessState {
        pid: 4242,
        port: 3080,
        cwd: "/tmp".into(),
        started_at: 7,
    };
    write_state(&dir, &state).unwrap();
    let back = read_state(&dir).unwrap();
    assert_eq!(back.pid, 4242);
    assert_eq!(back.port, 3080);
    clear_state(&dir);
    assert!(read_state(&dir).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
