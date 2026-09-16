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

/// A single line larger than the limit used to be appended to a file that was still under it,
/// leaving the log far above its target until the *next* line arrived.
#[test]
fn one_oversized_line_does_not_overshoot_the_limit() {
    let dir = std::env::temp_dir().join("dsh-desktop-log-overshoot-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("harness.log");
    let logger = Logger::with_limit(&path, 200);

    logger.write("small line");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 11);
    logger.write(&"z".repeat(5_000));

    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        5_001,
        "the oversized line belongs in a fresh file, not appended to the small one"
    );
    assert_eq!(
        std::fs::metadata(dir.join("harness.log.1")).unwrap().len(),
        11,
        "what was there before must be rotated out, not left in place"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Rotating an empty file would only shuffle the backups: the line still has to be written.
#[test]
fn an_empty_log_is_not_rotated_for_one_oversized_line() {
    let dir = std::env::temp_dir().join("dsh-desktop-log-empty-rotate-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("harness.log");
    let logger = Logger::with_limit(&path, 200);

    logger.write(&"z".repeat(5_000));

    assert!(path.is_file(), "the line must still be written somewhere");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 5_001);
    assert!(
        !dir.join("harness.log.1").exists(),
        "there was nothing to rotate"
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
    let url = parse_dsh_url("dsh web: http://127.0.0.1:59753/?token=abc", 59753).unwrap();
    assert_eq!(url.port(), Some(59753));
}

#[test]
fn ignores_lan_suffix() {
    let line = "dsh web: http://127.0.0.1:59753/?token=abc (LAN: http://10.0.0.5:59753/?token=abc)";
    let url = parse_dsh_url(line, 59753).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.port(), Some(59753));
}

#[test]
fn rejects_foreign_hosts_and_plain_lines() {
    assert!(parse_dsh_url("dsh web: http://example.com/?token=abc", 59753).is_none());
    assert!(parse_dsh_url("dsh web: listening", 59753).is_none());
    assert!(parse_dsh_url("hello", 59753).is_none());
    assert!(parse_dsh_url("open http://10.0.0.5:59753/?token=abc", 59753).is_none());
    // No token query: not a startup URL, however loopback it looks.
    assert!(parse_dsh_url("health http://127.0.0.1:59753/", 59753).is_none());
}

/// Upstream may reword or drop the `dsh web:` prefix; the parser must survive it.
#[test]
fn accepts_a_url_without_the_documented_prefix() {
    let url = parse_dsh_url(
        "harness listening on http://127.0.0.1:59753/?token=abc",
        59753,
    )
    .unwrap();
    assert_eq!(url.port(), Some(59753));
    // A reworded prefix with a LAN suffix still prefers the loopback URL.
    let line = "web ui: http://127.0.0.1:3080/?token=abc (LAN: http://10.0.0.5:3080/?token=abc)";
    assert_eq!(parse_dsh_url(line, 3080).unwrap().port(), Some(3080));
    // And a broken prefixed token must not stop the scan.
    let mixed = "dsh web: not-a-url | http://127.0.0.1:4123/?token=abc";
    assert_eq!(parse_dsh_url(mixed, 4123).unwrap().port(), Some(4123));
}

/// Without the prefix, only this launch's own port counts: a plugin or MCP server printing its
/// local address during startup used to be adopted as the Harness, and its port recorded in
/// `state.json`.
#[test]
fn the_prefixless_fallback_only_accepts_this_launchs_port() {
    let line = "sidecar listening on http://127.0.0.1:9999/?token=abc";
    assert!(
        parse_dsh_url(line, 3080).is_none(),
        "another port is another server"
    );
    assert_eq!(parse_dsh_url(line, 9999).unwrap().port(), Some(9999));
    // The prefixed form is the CLI speaking about itself: taken as printed.
    let prefixed = "dsh web: http://127.0.0.1:59753/?token=abc";
    assert_eq!(parse_dsh_url(prefixed, 3080).unwrap().port(), Some(59753));
}

#[test]
fn parses_ss_listener_pid() {
    let line = r#"LISTEN 0 511 127.0.0.1:3080 0.0.0.0:* users:(("node",pid=73596,fd=17))"#;
    assert_eq!(parse_ss_pid(line), Some(73596));
    assert_eq!(parse_ss_pid("LISTEN 0 511 127.0.0.1:3080 0.0.0.0:*"), None);
    assert_eq!(parse_ss_pid("users:((\"node\",pid=,fd=1))"), None);
}

/// `netstat -ano` in the shapes that matter: the state word is localized, and an outbound
/// connection to a remote server on the same port sorts before the loopback listener.
#[test]
fn parses_the_netstat_listener_and_nothing_else() {
    let outbound_first = "\
  Proto  Local Address          Foreign Address        State           PID
  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1000
  TCP    10.0.0.5:50123         93.184.216.34:3080     ESTABLISHED     4242
  TCP    127.0.0.1:3080         0.0.0.0:0              LISTENING       7777
  TCP    127.0.0.1:3080         127.0.0.1:50200        ESTABLISHED     7777
";
    assert_eq!(parse_netstat_listener(outbound_first, 3080), Some(7777));

    // German Windows, IPv6-only listener, with a UDP socket and a longer port number around it.
    let localized = "\
  Proto  Lokale Adresse         Remoteadresse          Status           PID
  TCP    127.0.0.1:13080        0.0.0.0:0              ABHÖREN          1111
  TCP    [::1]:3080             [::]:0                 ABHÖREN          7777
  UDP    0.0.0.0:3080           *:*                                     5555
";
    assert_eq!(parse_netstat_listener(localized, 3080), Some(7777));

    // Only connections, no listener: nothing to report.
    let connections =
        "  TCP    10.0.0.5:50123         93.184.216.34:3080     ESTABLISHED     4242\n";
    assert_eq!(parse_netstat_listener(connections, 3080), None);
    assert_eq!(parse_netstat_listener("", 3080), None);
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

/// The case the silent-peer test above does not cover, and the one that used to hang for ever.
///
/// A socket timeout bounds a single `read` call. A peer that puts a byte inside every window
/// therefore makes `read` return `Ok` indefinitely: reading to EOF never finishes and the buffer
/// grows without limit. Any streaming endpoint on the port is enough (SSE, a log tail, a long
/// poll), and the takeover and update paths *poll* this probe inside a loop they budget in
/// seconds — so a bound that never fires turns a five-second wait into an unbounded one.
#[test]
fn probe_gives_up_on_a_listener_that_keeps_sending() {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let stop_w = stop.clone();
    let sent_w = sent.clone();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        // A plausible answer that is not the fence, then an endless stream after it.
        if stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
            .is_err()
        {
            return;
        }
        while !stop_w.load(Ordering::SeqCst) {
            if stream.write_all(&[b'x'; 4 * 1024]).is_err() {
                return;
            }
            let _ = stream.flush();
            sent_w.fetch_add(4 * 1024, Ordering::SeqCst);
            // Comfortably inside PROBE_TIMEOUT, which is exactly what defeats a per-call
            // timeout and what the budget for the whole exchange is there to survive.
            std::thread::sleep(Duration::from_millis(100));
        }
    });

    let started = std::time::Instant::now();
    let verdict = probe(port);
    let elapsed = started.elapsed();
    stop.store(true, Ordering::SeqCst);

    assert_eq!(verdict, Probe::Other, "a 200 is not a Harness");
    // The status line settles the verdict, so there is nothing left to wait for.
    assert!(
        elapsed < Duration::from_millis(500),
        "probe waited on a peer that never stops sending: {elapsed:?}"
    );
    assert!(
        sent.load(Ordering::SeqCst) <= 8 * 1024,
        "probe kept reading after the verdict was already settled"
    );
}

#[test]
fn redacts_token() {
    let out = redact("dsh web: http://127.0.0.1:1/?token=abcdef (LAN: x)");
    assert!(out.contains("token=***"));
    assert!(!out.contains("abcdef"));
}

/// Every credential on the line must go, not just the first. The scan used to compare against the
/// wrong offset from the second match on, so an env dump or an echoed header kept every secret
/// after the first one.
#[test]
fn redacts_every_credential_on_the_line() {
    let cases = [
        ("token=aaa token=bbb", "aaa", "bbb"),
        (
            "DEEPSEEK_API_KEY=sk-first OPENAI_API_KEY=sk-second",
            "sk-first",
            "sk-second",
        ),
        ("x Bearer aaa, y Bearer bbb", "aaa", "bbb"),
        ("中文 token=a token=b token=c", "a", "b"),
    ];
    for (line, first, second) in cases {
        let out = redact(line);
        assert!(!out.contains(first), "first credential survived: {out}");
        assert!(!out.contains(second), "second credential survived: {out}");
    }
}

/// The offset bug also panicked: `line[..at]` is not a char boundary once a multi-byte character
/// precedes a match, and the panic landed in the reader thread or under the `APP_LOGGER` lock.
#[test]
fn redaction_survives_multi_byte_text_before_a_match() {
    for line in [
        "错误：token 无效，请检查 token=xxx",
        "中tokenXYtoken=1",
        "emoji ✨ api_key=1 api_key=2",
        "中文 Authorization: Bearer sk-1",
    ] {
        let out = redact(line);
        assert!(
            !out.contains("xxx") && !out.contains("sk-1"),
            "not redacted: {out}"
        );
    }
}

/// Redaction must not rewrite ordinary prose: the scheme names are common English words.
#[test]
fn redaction_leaves_prose_and_identifiers_alone() {
    for line in [
        "the token is expired",
        "tokenizer ready, tokens=5",
        "a basic example",
    ] {
        assert_eq!(redact(line), line, "prose was rewritten");
    }
}

/// `Basic`/`Digest` carry a credential in the same place as `Bearer`, and used to be left whole.
#[test]
fn redacts_the_other_authorization_schemes() {
    for line in [
        "Authorization: Basic dXNlcjpwYXNz",
        "Authorization: Digest user=1",
        "authorization: bearer sk-live-1",
    ] {
        let out = redact(line);
        for secret in ["dXNlcjpwYXNz", "user=1", "sk-live-1"] {
            assert!(!out.contains(secret), "{secret} survived: {out}");
        }
    }
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

/// A provider error echoes the request as JSON (or as an escaped JSON string, or a Python dict),
/// and the key is followed by its closing quote rather than by `=` or `:`.
#[test]
fn redacts_quoted_and_structured_fields() {
    let cases = [
        (
            r#"{"api_key":"sk-json","token":"t-json"}"#,
            &["sk-json", "t-json"][..],
        ),
        (
            r#"{"api_key": "sk json with spaces"}"#,
            &["sk json with spaces"][..],
        ),
        (
            r#"error: {\"api_key\":\"sk-escaped\"}"#,
            &["sk-escaped"][..],
        ),
        ("{'password': 'hunter2', 'user': 'me'}", &["hunter2"][..]),
        (r#"token: "quoted-value""#, &["quoted-value"][..]),
        ("password='single'", &["single"][..]),
        (
            r#"{"authorization": "Basic dXNlcjpwYXNz"}"#,
            &["dXNlcjpwYXNz"][..],
        ),
        (r#"{"secret": 12345}"#, &["12345"][..]),
        ("password = hunter2", &["hunter2"][..]),
        // A line cut inside a quoted value, or a quote that is never closed.
        (r#"{"api_key":"sk-cut-mid-valu"#, &["sk-cut-mid-valu"][..]),
        (r#"token: " sk-unterminated"#, &["sk-unterminated"][..]),
    ];
    for (line, secrets) in cases {
        let out = redact(line);
        for secret in secrets {
            assert!(!out.contains(secret), "{line} leaked {secret}: {out}");
        }
    }
}

/// The structure around a redacted value survives, so the log stays readable and parseable.
#[test]
fn quoted_redaction_keeps_the_structure() {
    assert_eq!(
        redact(r#"{"api_key":"sk-json","model":"deepseek"}"#),
        r#"{"api_key":"***","model":"deepseek"}"#
    );
    assert_eq!(
        redact(r#"{"password":"a\"b","n":1}"#),
        r#"{"password":"***","n":1}"#
    );
    // A quoted word that is not followed by a separator is prose, and a plural or longer key is
    // not the field. (`token = x` IS treated as a field: a config dump prints exactly that.)
    for line in [
        r#"the "token" was refused"#,
        r#"{"tokens": 5, "max_tokens": 10}"#,
        "user's token's value",
    ] {
        assert_eq!(redact(line), line, "prose or a count was rewritten");
    }
}

/// The scheme check used to lowercase the whole rest of the line once per match, which made a
/// line packed with fields quadratic. The bound is generous; the quadratic version needed several
/// seconds for this input even in a release build.
#[test]
fn redaction_stays_linear_on_a_line_full_of_fields() {
    let line = "token=a ".repeat(1024 * 1024 / 8);
    let started = std::time::Instant::now();
    let out = redact(&line);
    assert!(!out.contains("token=a"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
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

/// The cap has to be applied to what is read, not to what is kept: `BufRead::lines()` grows one
/// `String` until a newline arrives, so a single line with no newline in it is unbounded.
#[test]
fn an_oversized_line_is_capped_and_the_stream_keeps_going() {
    let mut input: Vec<u8> = vec![b'x'; LINE_LIMIT_BYTES * 3];
    input.extend_from_slice(b"\ntail line\n");
    let mut reader = BufReader::new(&input[..]);

    let first = read_line(&mut reader).unwrap();
    assert!(
        first.len() <= LINE_LIMIT_BYTES + TRUNCATION_NOTE.len(),
        "one line must not exceed the cap: {} bytes",
        first.len()
    );
    assert!(first.ends_with(TRUNCATION_NOTE), "a cut line must say so");
    // The rest of the oversized line is drained, not read as lines of its own.
    assert_eq!(read_line(&mut reader).as_deref(), Some("tail line"));
    assert_eq!(read_line(&mut reader), None);
}

/// Invalid UTF-8 used to end the stream silently: `lines().map_while(Result::ok)` stops at the
/// first `Err`, so every line after it — including the `dsh web:` line — was dropped.
#[test]
fn an_invalid_utf8_byte_does_not_end_the_stream() {
    let mut input: Vec<u8> = b"plugin says: ".to_vec();
    input.push(0xFF);
    input.extend_from_slice(b" oops\n");
    input.extend_from_slice(b"dsh web: http://127.0.0.1:59753/?token=abc\n");
    let mut reader = BufReader::new(&input[..]);

    assert!(read_line(&mut reader).unwrap().contains("plugin says"));
    let url_line = read_line(&mut reader).unwrap();
    assert!(
        parse_dsh_url(&url_line, 59753).is_some(),
        "the URL line after a malformed one must still arrive: {url_line:?}"
    );
}

/// An empty line is a line: ending the stream on one would drop everything after it.
#[test]
fn blank_lines_do_not_end_the_stream() {
    let mut reader = BufReader::new(&b"first\n\nthird\n"[..]);
    assert_eq!(read_line(&mut reader).as_deref(), Some("first"));
    assert_eq!(read_line(&mut reader).as_deref(), Some(""));
    assert_eq!(read_line(&mut reader).as_deref(), Some("third"));
    assert_eq!(read_line(&mut reader), None);
}

/// `BufRead::lines` strips the `\r` of a CRLF stream; the replacement must too.
#[test]
fn carriage_returns_are_stripped() {
    let mut reader = BufReader::new(&b"line\r\nnext\r\n"[..]);
    assert_eq!(read_line(&mut reader).as_deref(), Some("line"));
    assert_eq!(read_line(&mut reader).as_deref(), Some("next"));
    assert_eq!(read_line(&mut reader), None);
}

/// The failure page shows `Ring::tail()`: an unbounded ring would put an arbitrarily large string
/// on a page the user is waiting for.
#[test]
fn the_ring_is_bounded_by_bytes_as_well_as_lines() {
    let ring = Ring::new();
    let line = "y".repeat(LINE_LIMIT_BYTES);
    for _ in 0..64 {
        ring.push_redacted(&line);
    }
    assert!(
        ring.tail().len() <= RING_BYTES,
        "the ring tail must stay within its byte budget: {} bytes",
        ring.tail().len()
    );
}

/// Redaction happens in the reader, so the ring only ever holds redacted text.
#[test]
fn the_ring_keeps_what_the_reader_redacted() {
    let ring = Ring::new();
    ring.push_redacted(&redact("dsh web: http://127.0.0.1:1/?token=abcdef"));
    let tail = ring.tail();
    assert!(tail.contains("token=***") && !tail.contains("abcdef"));
}
