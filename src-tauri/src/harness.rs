//! Spawn `dsh web`, stream its output, parse the startup URL, and probe for a live instance.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

pub const URL_PREFIX: &str = "dsh web:";
const RING_CAPACITY: usize = 200;
/// Bytes the ring keeps across all its lines, so its tail stays a page-sized string.
const RING_BYTES: usize = 1024 * 1024;
/// Longest single line kept from the Harness output.
///
/// A CLI, node, or a plugin can print a line with no newline in it at all: a minified JSON
/// document, a base64 blob, a provider error with the request body inlined. Reading by line
/// grows one `String` until that newline arrives, and the finished line then goes on to the
/// ring, the redactor and the log. 256 KiB is orders of magnitude above a real log line and
/// still small enough that the whole path stays flat.
const LINE_LIMIT_BYTES: usize = 256 * 1024;
/// Appended to a line [`LINE_LIMIT_BYTES`] cut, so a partial line is never read as a whole one.
const TRUNCATION_NOTE: &str = " …[truncated: line exceeded 256 KiB]";
const LOG_LIMIT_BYTES: u64 = 5 * 1024 * 1024;
/// Rotated generations kept beside the live file (design §8: 5 MB x 3).
const LOG_BACKUPS: usize = 3;

/// Last [`RING_CAPACITY`] lines of Harness output, bounded by [`RING_BYTES`] as well.
///
/// The lines arrive already redacted: the reader redacts once and hands the same string to both
/// the ring and the log, so the scan is not repeated per sink.
#[derive(Clone)]
pub struct Ring(Arc<Mutex<Tail>>);

#[derive(Default)]
struct Tail {
    lines: VecDeque<String>,
    /// Sum of `lines` plus their separators, so eviction does not need to re-measure them.
    bytes: usize,
}

impl Default for Ring {
    fn default() -> Self {
        Ring::new()
    }
}

impl Ring {
    pub fn new() -> Self {
        Ring(Arc::new(Mutex::new(Tail {
            lines: VecDeque::with_capacity(RING_CAPACITY),
            bytes: 0,
        })))
    }

    /// Keep one already-redacted line, dropping the oldest until both bounds hold.
    pub fn push_redacted(&self, line: &str) {
        let mut tail = self.0.lock().unwrap();
        // A single line above the byte budget would otherwise evict everything and still be kept
        // whole; the reader caps lines at `LINE_LIMIT_BYTES`, so this only guards the bound.
        if line.len() > RING_BYTES {
            return;
        }
        while tail.lines.len() >= RING_CAPACITY || tail.bytes + line.len() + 1 > RING_BYTES {
            match tail.lines.pop_front() {
                Some(old) => tail.bytes -= old.len() + 1,
                None => break,
            }
        }
        tail.bytes += line.len() + 1;
        tail.lines.push_back(line.to_string());
    }

    /// The kept lines as one block, for the failure page. Never larger than [`RING_BYTES`].
    pub fn tail(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .lines
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Field names whose value must never reach the log file.
///
/// Matched case-insensitively anywhere in a line, including as the tail of a longer identifier
/// (`DEEPSEEK_API_KEY`, `my_token`), which is how these arrive from environment dumps and from
/// provider errors that echo the request back.
const SECRET_KEYS: &[&str] = &[
    "token",
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "cookie",
    "set-cookie",
    "password",
    "passwd",
    "secret",
    "private_key",
    "access_key",
    "session_id",
];

/// Where a value ends: the separators that follow one in a query string, a header, or an env dump.
const VALUE_END: &[char] = &['&', ';', ',', '"', '\'', ')', '}', ']', '<', '>', '|'];

/// Replace credential-bearing values with `***` so they never reach the log file.
///
/// The launch token was the first of these (`?token=…` in the startup URL), but it is not the only
/// one the supervised CLI and its plugins can print: provider errors echo request headers, an env
/// dump carries `DEEPSEEK_API_KEY`, and a failing fetch can log its cookies. Everything the log
/// receives goes through here, so this is the single place that has to know the field names.
pub fn redact(line: &str) -> String {
    let mut out = redact_bearer(line);
    for key in SECRET_KEYS {
        out = redact_field(&out, key);
    }
    out
}

/// `Bearer <credential>` -> `***`, whatever key introduced it.
///
/// Runs before the field scan because the credential does not sit directly behind a `=` or `:`:
/// `Authorization: Bearer sk-…` would otherwise have its value read as the word `Bearer` and the
/// secret left behind. The scheme word goes too, so the later field pass over `authorization`
/// finds nothing left to replace and cannot leave a stray `*** ***` behind.
fn redact_bearer(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut search = lower.as_str();
    while let Some(at) = search.find("bearer") {
        let after = at + "bearer".len();
        // Only a standalone word: `bearer` inside a longer identifier is not a scheme.
        let boundary = at == 0
            || !line[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        if !boundary {
            out.push_str(&rest[..after]);
            rest = &rest[after..];
            search = &search[after..];
            continue;
        }
        out.push_str(&rest[..at]);
        rest = &rest[after..];
        search = &search[after..];
        // Drop the whitespace between the scheme and the credential with it.
        let spaces = rest.len() - rest.trim_start().len();
        rest = &rest[spaces..];
        search = &search[spaces..];
        let end = rest
            .find(|c: char| c.is_whitespace() || VALUE_END.contains(&c))
            .unwrap_or(rest.len());
        out.push_str("***");
        rest = &rest[end..];
        search = &search[end..];
    }
    out.push_str(rest);
    out
}

/// `key<separator>value` -> `key<separator>***` for one field name.
fn redact_field(line: &str, key: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut search = lower.as_str();
    loop {
        let Some(at) = search.find(key) else {
            out.push_str(rest);
            return out;
        };
        let after = at + key.len();
        // The key must stand alone: `tokens` and `tokenizer` are not the field.
        let starts_a_word = at == 0
            || !line[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let separator = rest[after..].chars().next();
        let Some(separator) = separator.filter(|c| matches!(c, '=' | ':')) else {
            out.push_str(&rest[..after]);
            rest = &rest[after..];
            search = &search[after..];
            continue;
        };
        if !starts_a_word {
            out.push_str(&rest[..after]);
            rest = &rest[after..];
            search = &search[after..];
            continue;
        }
        // A header (`key: value`) or an env dump (`KEY=value`) separates the name from the value,
        // and often puts a space after the separator; that space must not be read as an empty
        // value, which would leave the real one in place.
        let mut value_at = after + separator.len_utf8();
        let after_separator = &rest[value_at..];
        value_at += after_separator.len() - after_separator.trim_start().len();
        let end = rest[value_at..]
            .find(|c: char| c.is_whitespace() || VALUE_END.contains(&c))
            .unwrap_or(rest.len() - value_at);
        out.push_str(&rest[..value_at]);
        out.push_str("***");
        rest = &rest[value_at + end..];
        search = &search[value_at + end..];
    }
}

/// Startup URL of the supervised CLI.
///
/// Documented shape: `dsh web: http://127.0.0.1:59753/?token=xxx[ (LAN: http://10.0.0.5:...)]`.
/// The prefix is what upstream prints today, so the parser must not depend on its exact wording:
/// when it is missing, any token on the line that looks like a loopback startup URL is accepted.
/// Everything else (scheme, host, the token query) is still validated, and the LAN URL upstream
/// appends is rejected on purpose because its host is not loopback.
pub fn parse_dsh_url(line: &str) -> Option<Url> {
    if let Some((_, rest)) = line.split_once(URL_PREFIX) {
        // Only the first whitespace-delimited token: a LAN suffix may follow.
        if let Some(url) = rest.split_whitespace().next().and_then(startup_url) {
            return Some(url);
        }
    }
    line.split_whitespace().find_map(startup_url)
}

/// A startup URL is loopback http carrying the launch token as a query parameter.
fn startup_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.query().is_some() {
        Some(url)
    } else {
        None
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    /// A Harness answered its auth fence.
    Harness,
    /// Something else owns the port.
    Other,
    /// Nothing is listening.
    Closed,
}

/// Bound every network touch: a peer that accepts the connection and then stays silent
/// must not be able to park the startup thread forever.
const PROBE_TIMEOUT: Duration = Duration::from_millis(600);

/// How much of a response the probe keeps before giving up on it.
///
/// The real fence is one short status line plus a 68-byte sentence. This cap is for a peer
/// that answers and then keeps sending: `PROBE_TIMEOUT` bounds the time, this
/// bounds the memory.
const PROBE_RESPONSE_LIMIT: usize = 64 * 1024;

/// Budget for the `ps` / PowerShell call that names the process behind a port.
///
/// Same reasoning as the probe budget: this runs while the user is looking at the splash page,
/// and a `ps` that never returns must not be able to park the takeover path.
const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// The sentence only the CLI auth fence carries: the one thing that identifies a Harness.
const AUTH_FENCE: &str = "dsh web authentication required";

/// Point the stream IO timeouts at whatever is left of `deadline`.
///
/// Returns false once the budget is gone, and that is what actually stops a peer which keeps
/// sending: a socket timeout applies to a single `read` call, so a peer that puts one
/// byte inside every window makes `read` return `Ok` for ever. Re-arming before each
/// call turns the per-call timeout into a budget for the whole exchange.
fn arm_within(stream: &TcpStream, deadline: Instant) -> bool {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return false;
    }
    let _ = stream.set_read_timeout(Some(left));
    let _ = stream.set_write_timeout(Some(left));
    true
}

/// Is this status line the 401 both readings of the fence depend on?
fn is_unauthorized(status: &str) -> bool {
    status.starts_with("HTTP/1.1 401") || status.starts_with("HTTP/1.0 401")
}

/// Dependency-free HTTP probe over loopback: the auth fence is the only thing that identifies a
/// Harness.
///
/// That fence is unconditional in the CLI: a request without the launch-token cookie is answered
/// `401` with the body `dsh web authentication required; reopen the URL printed by dsh web.`, and
/// this probe never sends a cookie. A `200` therefore proves the port is *not* a Harness — the
/// reading that used to be treated as "a Harness that already has a session". An unrelated server
/// that happened to listen on the port answered 200 and was signalled as if it were a Harness.
pub fn probe(port: u16) -> Probe {
    let addr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return Probe::Other,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return Probe::Closed,
    };
    // The connect timeout does not cover the exchange, so the exchange gets a deadline of
    // its own, re-armed before every read (see `arm_within`).
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let request = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: dsh-desktop\r\nConnection: close\r\n\r\n"
    );
    if !arm_within(&stream, deadline) || stream.write_all(request.as_bytes()).is_err() {
        return Probe::Other;
    }
    let mut response: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        if let Some(answer) = read_verdict(&response) {
            return answer;
        }
        if response.len() >= PROBE_RESPONSE_LIMIT || !arm_within(&stream, deadline) {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    read_verdict(&response).unwrap_or(Probe::Other)
}

/// The verdict the bytes so far already support, or `None` while the fence could still
/// arrive.
///
/// A status line that is not the `401` ends the exchange: `Connection: close` means a
/// server that answered anything else has said all it is going to, and waiting for it to
/// hang up only spends the budget it is holding.
fn read_verdict(response: &[u8]) -> Option<Probe> {
    let end = response.iter().position(|byte| *byte == b'\n')?;
    let status = String::from_utf8_lossy(&response[..end]);
    if !is_unauthorized(&status) {
        return Some(Probe::Other);
    }
    let body = String::from_utf8_lossy(&response[end..]);
    body.contains(AUTH_FENCE).then_some(Probe::Harness)
}

/// Does this command line name the `dsh web` server the CLI boots?
///
/// The auth fence says a *Harness* is on the port; this says the process behind it is the CLI
/// rather than some other program that answers 401 with the same words. Both are checked before a
/// process this shell did not start is signalled, so neither signal alone can get an unrelated
/// server killed.
///
/// `--profile web` is what this shell passes itself; a bare `web` token is the CLI's hardcoded
/// alias for it, which is how the server looks when a user starts it from a terminal. A `plugin`
/// command carries the same profile flag while managing the profile's dependencies, and it is not
/// a server, so it is excluded explicitly.
pub fn looks_like_dsh_web(command: &str) -> bool {
    if !command.contains("dsh") || command.contains("plugin") {
        return false;
    }
    command.contains("--profile web") || command.split_whitespace().any(|token| token == "web")
}

/// Command line of `pid`, when the platform lets us read it.
///
/// The auth fence proves *what* is on the port; this is the second, independent signal that
/// names *which program* is behind it. Consulted before signalling a process this shell did not
/// start, so a fence-shaped answer alone is not enough to get an unrelated server killed.
pub fn process_command(pid: u32) -> Option<String> {
    // `-ww` matters: the identifying flags sit behind a long node path, and some `ps`
    // builds truncate the command column to the terminal width without it. Bounded, because this
    // runs on the takeover path while the user waits on the splash page.
    #[cfg(unix)]
    let command = crate::process::stdout_within(
        std::process::Command::new("ps").args(["-ww", "-p", &pid.to_string(), "-o", "command="]),
        PS_TIMEOUT,
    )?;
    // `tasklist` reports only the image name, which cannot tell one node program from another.
    #[cfg(windows)]
    let command = crate::process::stdout_within(
        std::process::Command::new("powershell").args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine"),
        ]),
        PS_TIMEOUT,
    )?;
    (!command.is_empty()).then_some(command)
}

/// PID currently listening on `port`, when the platform lets us find it cheaply.
///
/// Only used after the auth fence already proved the port serves a Harness, so the
/// caller never kills an unidentified process.
pub fn listener_pid(port: u16) -> Option<u32> {
    #[cfg(unix)]
    {
        if let Some(pid) = listener_pid_lsof(port) {
            return Some(pid);
        }
        // Many Linux distributions ship without lsof; `ss` (iproute2) normally exists, so it
        // is the fallback that keeps the takeover path working there.
        #[cfg(target_os = "linux")]
        {
            if let Some(pid) = listener_pid_ss(port) {
                return Some(pid);
            }
        }
        None
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("netstat")
            .args(["-ano"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let needle = format!(":{port} ");
        for line in text.lines() {
            if line.contains("LISTENING") && line.contains(&needle) {
                if let Some(pid) = line.split_whitespace().last() {
                    return pid.parse().ok();
                }
            }
        }
        None
    }
}

#[cfg(unix)]
fn listener_pid_lsof(port: u16) -> Option<u32> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// `ss -ltnpH 'sport = :PORT'` line, e.g.
/// `LISTEN 0 511 127.0.0.1:3080 0.0.0.0:* users:(("node",pid=73596,fd=17))`.
#[cfg(target_os = "linux")]
fn listener_pid_ss(port: u16) -> Option<u32> {
    let out = std::process::Command::new("ss")
        .args(["-ltnpH", &format!("sport = :{port}")])
        .output()
        .ok()?;
    parse_ss_pid(&String::from_utf8_lossy(&out.stdout))
}

/// First `pid=<digits>` in `ss` output. Compiled everywhere so tests cover it on macOS too.
#[allow(dead_code)]
pub fn parse_ss_pid(output: &str) -> Option<u32> {
    let index = output.find("pid=")?;
    let digits: String = output[index + 4..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

pub struct Spawned {
    pub child: Child,
    urls: Receiver<Url>,
    pub ring: Ring,
}

pub struct SpawnOptions<'a> {
    pub workspace: &'a Path,
    pub overlay: &'a Path,
    pub dsh_home: Option<&'a Path>,
    pub port: u16,
    pub log_path: &'a Path,
    /// Extra environment for the child: imported login-shell vars, merged PATH, config overrides.
    pub env: &'a [(String, String)],
}

/// Launch `<node> <dsh.js> --profile web --patch <overlay> --no-open --port N`.
///
/// Launcher flags must precede app flags; the child gets its own process group so the
/// whole tree can be terminated together.
pub fn spawn(node: &Path, dsh_js: &Path, opts: &SpawnOptions<'_>) -> std::io::Result<Spawned> {
    // A relative path is resolved against the child's working directory, not ours, which turns
    // a bad PATH entry into an error from inside node (`EISDIR: lstat 'D:'`); a workspace that
    // does not exist fails with a bare ENOENT that never names the config entry. `validate_paths`
    // names the culprit before the child is started.
    validate_paths(node, dsh_js, opts.workspace)?;
    let mut command = Command::new(node);
    command
        .arg(dsh_js)
        .arg("--profile")
        .arg("web")
        .arg("--patch")
        .arg(opts.overlay)
        .arg("--no-open")
        .arg("--port")
        .arg(opts.port.to_string())
        .current_dir(opts.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.envs(opts.env.iter().cloned());
    if let Some(home) = opts.dsh_home {
        command.env("DSH_HOME", home);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    // A bug report needs the exact command line, and a bad workspace must be visible before
    // the child fails for a reason that never names it.
    app_log(&format!(
        "spawn: {} | cwd: {} | DSH_HOME: {}",
        describe(&command),
        opts.workspace.display(),
        opts.dsh_home
            .map(|home| home.display().to_string())
            .unwrap_or_else(|| "<default>".to_string()),
    ));

    let mut child = command.spawn()?;
    let (tx, rx) = mpsc::channel();
    let ring = Ring::new();
    let log = Logger::open(opts.log_path);
    if let Some(stdout) = child.stdout.take() {
        forward_lines(stdout, tx.clone(), ring.clone(), log.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward_lines(stderr, tx, ring.clone(), log);
    }
    Ok(Spawned {
        child,
        urls: rx,
        ring,
    })
}

impl Spawned {
    /// Block until the startup URL line arrives, or the timeout elapses.
    pub fn wait_for_url(&self, timeout: Duration) -> Result<Url, String> {
        match self.urls.recv_timeout(timeout) {
            Ok(url) => Ok(url),
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "等待启动 URL 超时（{:?}）。常见原因：profile 的 printUrl 被关掉、Harness 启动失败、首次初始化较慢。",
                timeout
            )),
            Err(RecvTimeoutError::Disconnected) => Err("Harness 输出已结束（进程已退出）。".to_string()),
        }
    }

    /// Give the child handle to the supervisor, so it can wait for the process to end.
    /// Consumes the value: the URL receiver belongs to the startup phase only.
    pub fn into_child(self) -> Child {
        self.child
    }
}

/// Every path handed to the child must be absolute, and the workspace must exist.
///
/// A relative path is resolved against the child's working directory rather than ours, so the
/// failure surfaces from inside node and never names the config entry behind it. Naming the
/// culprit here is what makes the failure page actionable (design §4/§5).
fn validate_paths(node: &Path, dsh_js: &Path, workspace: &Path) -> std::io::Result<()> {
    for (label, path) in [("node", node), ("dsh_js", dsh_js), ("workspace", workspace)] {
        if !path.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{label} 必须是绝对路径: {}", path.display()),
            ));
        }
    }
    if !workspace.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "workspace 不是已存在的目录（config.json 的 workspace）: {}",
                workspace.display()
            ),
        ));
    }
    Ok(())
}

/// Command line for the log: `Command` does not implement `Display`, and these arguments are
/// what a bug report needs. No credential is passed as a flag (the launch token is printed by
/// the child, and `app_log` redacts it).
fn describe(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().to_string()];
    parts.extend(command.get_args().map(|arg| {
        let arg = arg.to_string_lossy();
        if arg.contains(' ') {
            format!("\"{arg}\"")
        } else {
            arg.to_string()
        }
    }));
    parts.join(" ")
}

fn forward_lines<R: Read + Send + 'static>(
    reader: R,
    tx: mpsc::Sender<Url>,
    ring: Ring,
    log: Logger,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let Some(line) = read_line(&mut reader) else {
                return;
            };
            // One redaction for both sinks: the log and the failure page show the same text, and
            // the scan walks the whole line once per sensitive field.
            let safe = redact(&line);
            ring.push_redacted(&safe);
            log.write(&safe);
            // The URL is parsed from the line as the CLI printed it, never from the redacted
            // copy: redaction rewrites `token=…`, which is exactly the query the parser needs.
            if let Some(url) = parse_dsh_url(&line) {
                let _ = tx.send(url);
            }
        }
    });
}

/// One line, capped at [`LINE_LIMIT_BYTES`], or `None` once the stream ends.
///
/// Reading by line through `BufRead::lines()` has two failure modes this avoids. It grows one
/// `String` until a newline arrives, so a single line with no newline in it can be arbitrarily
/// large; and it returns `Err` on invalid UTF-8, which `map_while(Result::ok)` turns into a
/// silent, permanent end of the stream — the `dsh web:` line would never be parsed and the shell
/// would sit on the splash page until its timeout, with nothing in the log to explain it.
///
/// The cap is applied to the bytes read, before decoding, so an oversized line costs a fixed
/// buffer: the rest of it is drained and discarded. `from_utf8_lossy` keeps a partial or malformed
/// line readable instead of dropping every line that follows it.
fn read_line<R: BufRead>(reader: &mut R) -> Option<String> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    // Distinguishes "the stream ended" from "the line was empty": an empty line is a line, and
    // ending the stream on one would drop every line after it.
    let mut saw_line = false;
    loop {
        let available = match reader.fill_buf() {
            Ok([]) => break,
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let newline = available.iter().position(|byte| *byte == b'\n');
        let end = newline.unwrap_or(available.len());
        if !truncated {
            let room = LINE_LIMIT_BYTES.saturating_sub(bytes.len());
            let take = room.min(end);
            bytes.extend_from_slice(&available[..take]);
            if take < end {
                truncated = true;
            }
        }
        saw_line = true;
        match newline {
            Some(at) => {
                reader.consume(at + 1);
                break;
            }
            None => reader.consume(end),
        }
    }
    if !saw_line {
        return None;
    }
    let mut line = String::from_utf8_lossy(&bytes).into_owned();
    // `BufRead::lines` strips a trailing `\r`, so a CRLF stream reads the same here.
    if line.ends_with('\r') {
        line.pop();
    }
    if truncated {
        line.push_str(TRUNCATION_NOTE);
    }
    Some(line)
}

static APP_LOGGER: Mutex<Option<Logger>> = Mutex::new(None);

/// Point the shared app logger at the Harness log file (called once during startup).
pub fn init_app_log(path: &Path) {
    *APP_LOGGER.lock().unwrap() = Some(Logger::open(path));
}

/// Append a shell-side line (navigation blocks, downloads) to the same log, redacted.
pub fn app_log(line: &str) {
    if let Some(logger) = APP_LOGGER.lock().unwrap().as_ref() {
        logger.write(&format!("[dsh-desktop] {}", redact(line)));
    }
}

/// One `Logger` per path.
///
/// Two instances with independent file handles would keep appending to a renamed (rotated)
/// file, so the shell's own lines would land in a backup or vanish. `init_app_log` and `spawn`
/// both open the same log, so `open` hands out one shared handle per path.
static LOGGERS: Mutex<BTreeMap<PathBuf, Logger>> = Mutex::new(BTreeMap::new());

#[derive(Clone)]
pub struct Logger {
    path: PathBuf,
    /// Bytes allowed before the file rolls over; a field so tests can use a small limit.
    limit: u64,
    file: Arc<Mutex<Option<File>>>,
    /// Bytes in the live file, maintained by the single writer this process keeps per path.
    ///
    /// The alternative — stat the file before every line — would double the syscalls of the
    /// stream path, which design §13.6 budgets at one write per line.
    written: Arc<AtomicU64>,
}

impl Logger {
    pub fn open(path: &Path) -> Self {
        let mut loggers = LOGGERS.lock().unwrap();
        if let Some(existing) = loggers.get(path) {
            return existing.clone();
        }
        let logger = Logger::with_limit(path, LOG_LIMIT_BYTES);
        loggers.insert(path.to_path_buf(), logger.clone());
        logger
    }

    /// `open` with a limit a test can reach in a few lines. Also seeds the byte counter from the
    /// file on disk, so a file an earlier run left oversized is still rotated before the next
    /// line. Bypasses the registry: tests keep their own path.
    fn with_limit(path: &Path, limit: u64) -> Self {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        let written = std::fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        Logger {
            path: path.to_path_buf(),
            limit,
            file: Arc::new(Mutex::new(file)),
            written: Arc::new(AtomicU64::new(written)),
        }
    }

    pub fn write(&self, line: &str) {
        let mut guard = self.file.lock().unwrap();
        let incoming = line.len() as u64 + 1;
        self.rotate_before(&mut guard, incoming);
        if guard.is_none() {
            *guard = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok();
        }
        if let Some(file) = guard.as_mut() {
            if writeln!(file, "{line}").is_ok() {
                self.written.fetch_add(incoming, Ordering::Relaxed);
            }
        }
    }

    /// Roll the live file into `.1` when `incoming` bytes would not fit, keeping `LOG_BACKUPS`
    /// generations.
    ///
    /// The check includes the line about to be written, not just what is already on disk: a
    /// logger that only compared the current size would append a single oversized line to a file
    /// that is still under the limit and leave it far above it until the *next* line arrived.
    ///
    /// Driven by the byte counter, not by a stat per line: the counter is seeded when the logger
    /// opens, and this single writer per path keeps it exact, so the check still fires for a file
    /// the previous run left oversized.
    fn rotate_before(&self, guard: &mut Option<File>, incoming: u64) {
        let written = self.written.load(Ordering::Relaxed);
        if written + incoming <= self.limit {
            return;
        }
        // Rotating an empty file would only shuffle backups: the incoming line has to be written
        // somewhere, and a fresh file is the one place it fits even when it is oversized itself.
        if written == 0 {
            return;
        }
        // Drop the handle first: the next write reopens whatever ends up at `path`.
        *guard = None;
        for index in (1..LOG_BACKUPS).rev() {
            let _ = std::fs::rename(
                backup_path(&self.path, index),
                backup_path(&self.path, index + 1),
            );
        }
        let _ = std::fs::rename(&self.path, backup_path(&self.path, 1));
        self.written.store(0, Ordering::Relaxed);
    }
}

/// `harness.log` at index 1 becomes `harness.log.1`.
fn backup_path(path: &Path, index: usize) -> PathBuf {
    path.with_extension(format!("log.{index}"))
}

#[cfg(test)]
mod tests;
