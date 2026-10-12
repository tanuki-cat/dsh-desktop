//! Unit tests for the process module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;

#[cfg(unix)]
#[test]
fn short_helpers_do_not_receive_implicit_node_hooks() {
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "printf '%s' \"${NODE_OPTIONS-unset}\""]);
    command.env("NODE_OPTIONS", "--require inherited");
    assert_eq!(
        stdout_within(&mut command, Duration::from_secs(5)).as_deref(),
        Some("unset")
    );
}

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
    remember_spawned(pid);

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

/// A node Harness spawns workers and MCP children into its process group, and they outlive the
/// leader. Returning early when the leader is already gone left them running while the shell
/// believed it had cleaned up.
#[cfg(unix)]
#[test]
fn terminate_cleans_the_group_after_the_leader_is_gone() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    // The leader exits at once; the child stays in its group.
    let mut leader = Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 30 & exit 0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = leader.id();
    // Registered immediately after spawn: the group signal is only
    // allowed for a pid this process created, so an unregistered one is the *other* test below.
    remember_spawned(pgid);
    leader.wait().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !is_alive(pgid),
        "the leader must be gone for this test to mean anything"
    );

    terminate(pgid, Duration::from_millis(200));

    // The group is gone once the surviving child has been signalled and exited.
    std::thread::sleep(Duration::from_millis(300));
    let group = unsafe { libc::kill(-(pgid as i32), 0) };
    assert_eq!(group, -1, "the group still has a live member");
}

/// A pid this shell did not create must never reach a group signal.
///
/// `terminate` used to signal `-pid` whenever the leader was gone, whoever the leader had been.
/// Pids are reusable and so are process group ids, so a stale record — one left by an earlier
/// launch, or by a reboot — could address a group belonging to somebody else entirely. The
/// registry is what separates "our child, whose group may still hold workers" from "a number".
#[cfg(unix)]
#[test]
fn an_unregistered_pid_is_not_signalled_as_a_group() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut leader = Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 30 & exit 0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = leader.id();
    leader.wait().unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert!(!spawned_here(pgid), "this test needs an unregistered pid");
    terminate(pgid, Duration::from_millis(200));

    // The surviving child is still there: nothing was signalled.
    std::thread::sleep(Duration::from_millis(300));
    let group = unsafe { libc::kill(-(pgid as i32), 0) };
    assert_eq!(group, 0, "未登记的 pid 不得被当作进程组信号");
    // Clean up after the assertion, so the sleep does not outlive the test.
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
}

/// The budget has to bound the wait for a finished child's *output*, not just for the child.
///
/// A login shell exits at once, but a background process its rc file started inherits stdout and
/// keeps the pipe open. Joining the reader then waits for that process — so a one-second budget
/// became an unbounded wait on the startup thread.
#[cfg(unix)]
#[test]
fn a_child_that_leaves_the_pipe_open_does_not_stall_the_budget() {
    let started = Instant::now();
    let answer = stdout_within(
        std::process::Command::new("/bin/sh").args(["-c", "sleep 30 & echo /usr/local/bin/npm"]),
        Duration::from_secs(1),
    );
    let elapsed = started.elapsed();
    assert_eq!(answer.as_deref(), Some("/usr/local/bin/npm"));
    assert!(elapsed < Duration::from_secs(2), "waited {elapsed:?}");
}

/// A child that really hangs is still killed at its budget, and reports no answer.
#[cfg(unix)]
#[test]
fn a_child_that_never_finishes_is_killed_at_its_budget() {
    let started = Instant::now();
    let answer = stdout_within(
        std::process::Command::new("/bin/sh").args(["-c", "sleep 60"]),
        Duration::from_secs(1),
    );
    assert_eq!(answer, None);
    assert!(started.elapsed() < Duration::from_secs(3));
}

/// A helper that prints a document instead of a line is capped, and the cap is reported.
#[cfg(unix)]
#[test]
fn helper_output_is_capped_and_still_drained() {
    let out = output_within(
        std::process::Command::new("/bin/sh").args(["-c", "yes | head -c 3000000; echo"]),
        Duration::from_secs(30),
        4096,
    )
    .expect("the child exits normally");
    assert_eq!(out.text.len(), 4096);
    assert!(out.truncated);
}

/// A non-UTF-8 byte must not empty the result: the login-shell import used to lose every variable,
/// including the API key it exists to carry.
#[cfg(unix)]
#[test]
fn non_utf8_output_is_decoded_lossily() {
    let script = format!("printf 'DEEPSEEK_API_KEY=sk\\nLANG={}\\n'", '\u{ff}');
    let out = output_within(
        std::process::Command::new("/bin/sh").args(["-c", &script]),
        Duration::from_secs(5),
        65536,
    )
    .expect("the child exits normally");
    assert!(
        out.text.contains("DEEPSEEK_API_KEY=sk"),
        "got {:?}",
        out.text
    );
}

#[cfg(unix)]
struct TestChild(std::process::Child);
#[cfg(unix)]
impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        forget_spawned(self.0.id());
    }
}

#[cfg(unix)]
fn test_child() -> TestChild {
    TestChild(
        std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap(),
    )
}

#[cfg(unix)]
#[test]
fn birth_identity_is_stable_and_distinct_without_clock_sleeps() {
    let child = test_child();
    let other = test_child();
    let first = pin(child.0.id()).expect("OS process identity");
    assert!(same_process(&first));
    assert_ne!(first.marker, pin(other.0.id()).unwrap().marker);
}

#[cfg(unix)]
#[test]
fn stale_or_missing_identity_never_signals_a_live_process() {
    let mut child = test_child();
    let pid = child.0.id();
    let stale = Identity {
        pid,
        marker: "not-this-process".to_string(),
    };
    assert!(!same_process(&stale));
    assert!(!terminate_pid(&stale, Duration::ZERO));
    assert!(child.0.try_wait().unwrap().is_none());
    assert!(
        !terminate(pid, Duration::ZERO),
        "unregistered process must not receive a group signal"
    );
    let state = HarnessState {
        pid,
        port: 3080,
        cwd: "/tmp".into(),
        started_at: 0,
        started_marker: None,
    };
    assert!(!recorded_matches(&state));
}

#[cfg(unix)]
#[test]
fn adopted_identity_is_not_a_spawned_group_and_success_unregisters() {
    let mut child = test_child();
    let pid = child.0.id();
    remember_adopted(pin(pid).unwrap());
    assert!(!spawned_here(pid));
    assert!(terminate(pid, Duration::from_secs(2)));
    assert!(tracked(pid).is_none());
    let _ = child.0.wait();
}

#[test]
fn invalid_state_fields_are_rejected() {
    let dir = crate::test_dir("state-validation");
    std::fs::create_dir_all(&dir).unwrap();
    for raw in [
        r#"{"pid":0,"port":3080,"cwd":"/tmp","started_at":0}"#,
        r#"{"pid":4294967295,"port":3080,"cwd":"/tmp","started_at":0}"#,
        r#"{"pid":4242,"port":0,"cwd":"/tmp","started_at":0}"#,
        r#"{"pid":4242,"port":3080,"cwd":"relative","started_at":0}"#,
    ] {
        std::fs::write(state_path(&dir), raw).unwrap();
        assert!(read_state(&dir).is_none());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn state_write_does_not_follow_the_old_fixed_temporary_symlink() {
    use std::os::unix::{fs::symlink, fs::PermissionsExt};
    let dir = crate::test_dir("state-symlink");
    std::fs::create_dir_all(&dir).unwrap();
    let victim = dir.join("victim");
    std::fs::write(&victim, "important").unwrap();
    symlink(&victim, dir.join("state.json.tmp")).unwrap();
    let state = HarnessState {
        pid: 4242,
        port: 3080,
        cwd: "/tmp".into(),
        started_at: 0,
        started_marker: None,
    };
    write_state(&dir, &state).unwrap();
    write_state(&dir, &state).unwrap();
    assert_eq!(std::fs::read_to_string(victim).unwrap(), "important");
    assert_eq!(
        std::fs::metadata(state_path(&dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn state_round_trip() {
    let dir = crate::test_dir("dsh-desktop-state-test");
    let _ = std::fs::remove_dir_all(&dir);
    let state = HarnessState {
        pid: 4242,
        port: 3080,
        cwd: dir.to_string_lossy().into_owned(),
        started_at: 7,
        started_marker: None,
    };
    write_state(&dir, &state).unwrap();
    let back = read_state(&dir).unwrap();
    assert_eq!(back.pid, 4242);
    assert_eq!(back.port, 3080);
    assert_eq!(back.cwd, state.cwd);
    clear_state(&dir);
    assert!(read_state(&dir).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A pipe that is still open when the drain gives up leaves the last line unfinished. That line
/// must be dropped, not returned: in an `env` dump it can be an API key cut mid-way.
#[cfg(unix)]
#[test]
fn an_unfinished_last_line_is_dropped_when_the_pipe_stays_open() {
    let out = output_within(
        std::process::Command::new("/bin/sh")
            .args(["-c", "printf 'A=1\\nB=partial'; sleep 5 & exit 0"]),
        Duration::from_secs(5),
        65536,
    )
    .expect("the child exits normally");
    assert_eq!(out.text, "A=1\n");
    assert!(out.truncated);
}

/// When the one line a helper printed cannot be kept whole, there is no answer — not an empty one,
/// which callers read as "no such process".
#[cfg(unix)]
#[test]
fn a_helper_whose_only_line_was_cut_has_no_answer() {
    let answer = stdout_within(
        std::process::Command::new("/bin/sh").args(["-c", "printf partial; sleep 5 & exit 0"]),
        Duration::from_secs(5),
    );
    assert_eq!(answer, None);
    // The capped case reads the same way.
    let out = output_within(
        std::process::Command::new("/bin/sh").args(["-c", "printf 'KEY=%05000d' 0"]),
        Duration::from_secs(5),
        1024,
    )
    .expect("the child exits normally");
    assert_eq!(out.text, "");
    assert!(out.truncated);
}

#[test]
fn complete_lines_keeps_only_finished_lines() {
    assert_eq!(complete_lines(b"a\nb\nc"), b"a\nb\n");
    assert_eq!(complete_lines(b"a\n"), b"a\n");
    assert_eq!(complete_lines(b"partial"), b"");
    assert_eq!(complete_lines(b""), b"");
}
