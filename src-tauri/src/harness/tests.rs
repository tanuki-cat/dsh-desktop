//! Unit tests for the harness module.
//!
//! A child module of the code under test, so the parent glob import reaches
//! private items.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn spawn_paths_are_named_in_the_error() {
    let dir = std::env::temp_dir().join("dsh-desktop-spawn-validate-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let node = dir.join("node");
    let js = dir.join("bin.js");
    std::fs::write(&node, "").unwrap();
    std::fs::write(&js, "").unwrap();

    assert!(validate_paths(&node, &js, &dir).is_ok());

    let error = validate_paths(Path::new("node"), &js, &dir)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("node"),
        "the failing entry must be named: {error}"
    );
    let error = validate_paths(&node, Path::new("bin.js"), &dir)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("dsh_js"),
        "the failing entry must be named: {error}"
    );
    let error = validate_paths(&node, &js, Path::new("relative"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("workspace"),
        "the failing entry must be named: {error}"
    );

    // The message a user sees on the failure page must point at the config entry.
    let error = validate_paths(&node, &js, &dir.join("missing"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("workspace"), "{error}");
    assert!(error.contains("config.json"), "{error}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rotates_while_running_and_keeps_three_backups() {
    let dir = std::env::temp_dir().join("dsh-desktop-log-rotation-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("harness.log");
    // A small limit keeps the test fast; `with_limit` skips the shared registry so this
    // test cannot disturb (or be disturbed by) another test on the same path.
    let logger = Logger::with_limit(&path, 200);
    for index in 0..40 {
        logger.write(&format!("line {index:02} {}", "x".repeat(40)));
    }

    // The live file stays near the limit instead of holding the whole session.
    assert!(std::fs::metadata(&path).unwrap().len() <= 260);
    assert!(dir.join("harness.log.1").is_file());
    assert!(dir.join("harness.log.2").is_file());
    assert!(dir.join("harness.log.3").is_file());
    assert!(
        !dir.join("harness.log.4").exists(),
        "only {LOG_BACKUPS} backups are kept"
    );
    let newest = std::fs::read_to_string(dir.join("harness.log.1")).unwrap();
    assert!(
        newest.contains("line "),
        "the newest backup must hold log lines"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_below_the_limit_never_rotates() {
    let dir = std::env::temp_dir().join("dsh-desktop-log-small-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("harness.log");
    let logger = Logger::with_limit(&path, 200);
    for index in 0..3 {
        logger.write(&format!("line {index}"));
    }
    assert!(!dir.join("harness.log.1").exists());
    assert!(std::fs::read_to_string(&path).unwrap().contains("line 2"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_oversized_file_rotates_before_the_next_line() {
    let dir = std::env::temp_dir().join("dsh-desktop-log-leftover-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("harness.log");
    std::fs::write(&path, "y".repeat(300)).unwrap();

    let logger = Logger::with_limit(&path, 200);
    logger.write("fresh line");

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh line\n");
    assert_eq!(
        std::fs::read_to_string(dir.join("harness.log.1"))
            .unwrap()
            .len(),
        300
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two writers with their own handles would keep appending to a renamed file after the other
/// one rotates it, which loses the shell's own log lines.
#[test]
fn open_shares_one_logger_per_path() {
    let dir = std::env::temp_dir().join("dsh-desktop-log-share-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("harness.log");

    let first = Logger::open(&path);
    let second = Logger::open(&path);
    assert!(Arc::ptr_eq(&first.file, &second.file));
    assert_eq!(first.limit, LOG_LIMIT_BYTES);

    first.write("from first");
    second.write("from second");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("from first") && text.contains("from second"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parses_plain_url() {
    let url = parse_dsh_url("dsh web: http://127.0.0.1:59753/?token=abc").unwrap();
    assert_eq!(url.port(), Some(59753));
}

#[test]
fn ignores_lan_suffix() {
    let line = "dsh web: http://127.0.0.1:59753/?token=abc (LAN: http://10.0.0.5:59753/?token=abc)";
    let url = parse_dsh_url(line).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.port(), Some(59753));
}

#[test]
fn rejects_foreign_hosts_and_plain_lines() {
    assert!(parse_dsh_url("dsh web: http://example.com/?token=abc").is_none());
    assert!(parse_dsh_url("dsh web: listening").is_none());
    assert!(parse_dsh_url("hello").is_none());
    assert!(parse_dsh_url("open http://10.0.0.5:59753/?token=abc").is_none());
    // No token query: not a startup URL, however loopback it looks.
    assert!(parse_dsh_url("health http://127.0.0.1:59753/").is_none());
}

/// Upstream may reword or drop the `dsh web:` prefix; the parser must survive it.
#[test]
fn accepts_a_url_without_the_documented_prefix() {
    let url = parse_dsh_url("harness listening on http://127.0.0.1:59753/?token=abc").unwrap();
    assert_eq!(url.port(), Some(59753));
    // A reworded prefix with a LAN suffix still prefers the loopback URL.
    let line = "web ui: http://127.0.0.1:3080/?token=abc (LAN: http://10.0.0.5:3080/?token=abc)";
    assert_eq!(parse_dsh_url(line).unwrap().port(), Some(3080));
    // And a broken prefixed token must not stop the scan.
    let mixed = "dsh web: not-a-url | http://127.0.0.1:4123/?token=abc";
    assert_eq!(parse_dsh_url(mixed).unwrap().port(), Some(4123));
}

#[test]
fn parses_ss_listener_pid() {
    let line = r#"LISTEN 0 511 127.0.0.1:3080 0.0.0.0:* users:(("node",pid=73596,fd=17))"#;
    assert_eq!(parse_ss_pid(line), Some(73596));
    assert_eq!(parse_ss_pid("LISTEN 0 511 127.0.0.1:3080 0.0.0.0:*"), None);
    assert_eq!(parse_ss_pid("users:((\"node\",pid=,fd=1))"), None);
}

#[test]
fn probe_gives_up_on_a_silent_listener() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let silent = std::thread::spawn(move || {
        // Accept and stay silent: without an IO timeout the probe would wait here.
        if let Ok((stream, _)) = listener.accept() {
            std::thread::sleep(Duration::from_millis(1_200));
            drop(stream);
        }
    });
    let started = std::time::Instant::now();
    assert_eq!(probe(port), Probe::Other);
    assert!(
        started.elapsed() < Duration::from_millis(1_000),
        "probe waited for a silent peer"
    );
    let _ = silent.join();
}

#[test]
fn redacts_token() {
    let out = redact("dsh web: http://127.0.0.1:1/?token=abcdef (LAN: x)");
    assert!(out.contains("token=***"));
    assert!(!out.contains("abcdef"));
}

/// The launch token was only the first credential the log could receive: a provider error
/// echoes request headers, and an env dump carries the API key.
#[test]
fn redacts_the_credentials_a_provider_error_can_echo() {
    let cases = [
        ("Authorization: Bearer sk-live-123456", "sk-live-123456"),
        ("authorization: bearer sk-live-123456", "sk-live-123456"),
        ("DEEPSEEK_API_KEY=sk-abc=def", "sk-abc=def"),
        ("api_key: sk-abc", "sk-abc"),
        ("Cookie: session=deadbeef", "deadbeef"),
        ("set-cookie: sid=deadbeef; Path=/", "deadbeef"),
        ("password=hunter2", "hunter2"),
        ("client_secret: shhh", "shhh"),
    ];
    for (line, secret) in cases {
        let out = redact(line);
        assert!(!out.contains(secret), "{line} leaked {secret}: {out}");
        assert!(out.contains("***"), "{line} was not redacted: {out}");
    }
}

/// Redaction must not eat the surrounding line: the log is still meant to be readable.
#[test]
fn redaction_keeps_the_rest_of_the_line() {
    assert_eq!(
        redact("GET /v1/chat?token=abc&model=deepseek"),
        "GET /v1/chat?token=***&model=deepseek"
    );
    assert_eq!(redact("nothing secret here"), "nothing secret here");
    // A word that merely contains a key name is left alone.
    assert_eq!(redact("tokenizer loaded"), "tokenizer loaded");
    assert_eq!(redact("tokens=3"), "tokens=3");
}

/// The regression this probe exists for: an unrelated server that answers 200 must never be
/// mistaken for a Harness. Treating 200 as "a Harness that already has a session" is what let
/// a startup path signal whatever happened to hold the port.
#[test]
fn an_unrelated_http_server_is_not_a_harness() {
    for response in [
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html>vite dev server</html>",
        "HTTP/1.0 200 OK\r\n\r\n{}",
        "HTTP/1.1 401 Unauthorized\r\n\r\nlogin required",
        "HTTP/1.1 403 Forbidden\r\n\r\n",
        "HTTP/1.1 404 Not Found\r\n\r\n",
    ] {
        let (port, drained) = serve_once(response);
        assert_eq!(probe(port), Probe::Other, "misread: {response:?}");
        assert_drained(&drained);
    }
}

/// And the fence the CLI really serves still identifies it, so narrowing the probe did not
/// cost the takeover path its ability to recognise a Harness.
#[test]
fn the_auth_fence_identifies_a_harness() {
    let response = "HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\n\r\n\
                    dsh web authentication required; reopen the URL printed by dsh web.\n";
    let (port, drained) = serve_once(response);
    assert_eq!(probe(port), Probe::Harness);
    assert_drained(&drained);
}

/// A command line has to name the `dsh web` server before anything is signalled. Both the
/// flag this shell passes and the CLI's own `web` alias count.
#[test]
fn only_a_dsh_web_command_line_identifies_the_listener() {
    assert!(looks_like_dsh_web(
        "/opt/homebrew/bin/node /opt/homebrew/lib/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web --port 3080"
    ));
    assert!(looks_like_dsh_web("/usr/local/bin/dsh web --port 3080"));
    // A dsh doing something else is not the server this shell supervises.
    assert!(!looks_like_dsh_web("/opt/homebrew/bin/node dsh --version"));
    assert!(!looks_like_dsh_web(
        "dsh plugin --profile web add dshmarket"
    ));
    // And neither is an unrelated server that happens to hold the port.
    assert!(!looks_like_dsh_web("/usr/bin/python3 -m http.server 3080"));
    assert!(!looks_like_dsh_web(
        "node /srv/vite/bin/vite.js --port 3080"
    ));
}

/// Serve one canned HTTP response on a loopback port for a single probe.
///
/// Returns the port and a flag the serving thread sets once it has drained the whole request.
///
/// The drain is what the flag exists to prove. Closing a socket that still has unread bytes in
/// its receive buffer sends an RST instead of a FIN, and Windows discards what the peer already
/// received when it sees one. A write-only server therefore answered with an empty body *there*
/// while behaving on macOS — so the bug reached CI as a Windows-only failure, and the 200 test
/// passed for the wrong reason, since an empty body is `Other` too.
///
/// Asserting the flag rather than the probe result is deliberate: on macOS the RST still
/// delivers the body, so a platform-dependent outcome cannot catch a regression here. The flag
/// fails wherever it runs.
fn serve_once(response: &str) -> (u16, Arc<AtomicBool>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let response = response.to_string();
    let drained = Arc::new(AtomicBool::new(false));
    let served = drained.clone();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("the probe must connect");
        // Read the request to its end first: leaving it unread is what turns the close into
        // an RST, and an RST is what loses the response on Windows.
        let mut reader = BufReader::new(stream.try_clone().expect("clone for reading"));
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    // The blank line ends the request headers; the probe sends no body.
                    if line == "\r\n" || line == "\n" {
                        served.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let mut stream = stream;
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        // A plain close now that nothing is unread: FIN, not RST.
        let _ = stream.shutdown(std::net::Shutdown::Write);
    });
    (port, drained)
}

/// The request must reach the test server whole, on every platform.
fn assert_drained(drained: &AtomicBool) {
    assert!(
        drained.load(std::sync::atomic::Ordering::SeqCst),
        "the probe request must be drained before the socket closes, or the peer sees an RST\n\
         and Windows then discards the response"
    );
}
