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

/// Authentication schemes whose credential sits in the token after the scheme name.
///
/// Consulted only where a field name already proved the text is a credential (`Authorization:`),
/// never on free prose: `token`, `basic` and `digest` are ordinary English words, and scanning for
/// them anywhere would rewrite "the token is expired" into "the *** expired". `Bearer` is the one
/// exception — it is not an English word, so [`redact_bearer`] scans for it standalone too.
const AUTH_SCHEMES: &[&str] = &["bearer", "basic", "digest", "token"];

/// `Bearer <credential>` -> `***`.
///
/// Runs before the field scan because the credential does not sit directly behind a `=` or `:`:
/// `Authorization: Bearer sk-…` would otherwise have its value read as the word `Bearer` and the
/// secret left behind. The scheme word goes too, so the later field pass over `authorization`
/// finds nothing left to replace and cannot leave a stray `*** ***` behind.
///
/// Only `Bearer`, and only here: the other scheme names are ordinary words, so scanning free text
/// for them would rewrite prose. They are handled in [`redact_field`], where a field name has
/// already established that what follows is a credential.
fn redact_bearer(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    // Byte offsets into the ORIGINAL line. `rest` is a suffix of `line`, so a match found at `at`
    // inside `search` (= `rest` lowercased, same byte layout) sits at `line.len() - rest.len() + at`.
    // Looking backwards from `at` instead reads the wrong character from the second match on, and
    // `line[..at]` panics outright when `at` is not a char boundary — which any multi-byte
    // character earlier in the line makes it.
    let mut start = 0;
    loop {
        let rest = &line[start..];
        let search = &lower[start..];
        let Some(at) = search.find("bearer") else {
            out.push_str(rest);
            return out;
        };
        let absolute = start + at;
        let after = absolute + "bearer".len();
        // Only a standalone word: `bearer` inside a longer identifier is not a scheme.
        let boundary = absolute == 0
            || !line[..absolute]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        if !boundary {
            out.push_str(&line[start..after]);
            start = after;
            continue;
        }
        out.push_str(&line[start..absolute]);
        // Drop the whitespace between the scheme and the credential with it.
        let credential = line[after..].trim_start();
        let end = credential
            .find(|c: char| c.is_whitespace() || VALUE_END.contains(&c))
            .unwrap_or(credential.len());
        out.push_str("***");
        start = line.len() - credential.len() + end;
    }
}

/// Bytes of a leading authentication scheme in `value`, whitespace included, or 0.
///
/// Only called once a field name has proved `value` is a credential, so matching an ordinary
/// word here is safe: in `Authorization: Digest …` the word really is a scheme.
///
/// Compares the leading bytes in place. Lowercasing `value` instead would copy the whole rest of
/// the line once per match, which turned a 256 KiB line packed with `token=…` pairs into a
/// quadratic scan (measured: 180 ms for one line).
fn scheme_prefix_len(value: &str) -> usize {
    let bytes = value.as_bytes();
    for scheme in AUTH_SCHEMES {
        let len = scheme.len();
        if bytes.len() <= len || !bytes[..len].eq_ignore_ascii_case(scheme.as_bytes()) {
            continue;
        }
        // An ASCII match ends on a char boundary, so the slice is valid.
        let rest = &value[len..];
        // The scheme is a whole token: `Bearerish` is not `Bearer`.
        if rest.starts_with(char::is_whitespace) {
            return len + (rest.len() - rest.trim_start().len());
        }
    }
    0
}

/// How a located value is delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delimiter {
    /// Unquoted: the value runs to whitespace or one of [`VALUE_END`].
    Bare,
    /// Quoted: the value runs to the matching closing quote.
    Quoted(&'static str),
}

/// Quote tokens a key or a value can be wrapped in. `\"` comes first: a JSON document embedded in
/// a log string (`{\"api_key\":\"sk-…\"}`) escapes its quotes, and the bare `"` would otherwise be
/// matched one byte too late.
const QUOTES: &[&str] = &["\\\"", "\"", "'"];

fn quote_at(line: &str, at: usize) -> Option<&'static str> {
    let rest = &line[at..];
    QUOTES.iter().copied().find(|quote| rest.starts_with(quote))
}

fn skip_whitespace(line: &str, at: usize) -> usize {
    let rest = &line[at..];
    at + (rest.len() - rest.trim_start().len())
}

/// Where the value behind a key match starts, when the text after the key is a field separator.
///
/// Three shapes are fields:
///
/// - `KEY=value` and `key: value` — an env dump, a header, a query string;
/// - `key = value` — a config dump; whitespace is only allowed before `=`, because `word :` is
///   not how anything prints a field while `word = x` is;
/// - `"key": "value"` — JSON, a JS object, or a Python dict (`'key': 'value'`), which is the form
///   a provider error echoes a request in. The key's closing quote comes first.
///
/// Whitespace after the separator is skipped: `key: value` must not read as an empty value, which
/// would leave the real one in place.
fn locate_value(line: &str, after: usize) -> Option<(usize, Delimiter)> {
    let mut at = after;
    if let Some(quote) = quote_at(line, at) {
        at = skip_whitespace(line, at + quote.len());
        if !line[at..].starts_with(':') {
            return None;
        }
        at += 1;
    } else if line[at..].starts_with(['=', ':']) {
        at += 1;
    } else {
        let spaced = skip_whitespace(line, at);
        if spaced == at || !line[spaced..].starts_with('=') {
            return None;
        }
        at = spaced + 1;
    }
    at = skip_whitespace(line, at);
    match quote_at(line, at) {
        Some(quote) => Some((at + quote.len(), Delimiter::Quoted(quote))),
        None => Some((at, Delimiter::Bare)),
    }
}

/// Bytes of `value` up to where it ends.
fn value_len(value: &str, delimiter: Delimiter) -> usize {
    // Leading whitespace is skipped first. A bare value never has any (the separator scan skipped
    // it), but an unterminated quote falls back to these rules from just inside the quote, and
    // `" sk-…` would otherwise end at that space and leave the credential in place.
    let bare = || {
        let trimmed = value.trim_start();
        let skipped = value.len() - trimmed.len();
        skipped
            + trimmed
                .find(|c: char| c.is_whitespace() || VALUE_END.contains(&c))
                .unwrap_or(trimmed.len())
    };
    match delimiter {
        Delimiter::Bare => bare(),
        // A plain `"` escaped with a backslash is part of the value, not its end.
        Delimiter::Quoted("\"") => value
            .match_indices('"')
            .map(|(at, _)| at)
            .find(|at| !value[..*at].ends_with('\\'))
            .unwrap_or_else(bare),
        // No closing quote on this line: fall back to the bare rules rather than redact the rest
        // of the line, which would take unrelated text with it.
        Delimiter::Quoted(quote) => value.find(quote).unwrap_or_else(bare),
    }
}

/// `key<separator>value` -> `key<separator>***` for one field name.
///
/// Every match is scanned for, not just the first: an environment dump or a provider error that
/// echoes the request can carry several credentials on one line, and stopping at the first would
/// leave the rest in the log.
fn redact_field(line: &str, key: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    // Byte offsets into the ORIGINAL line; see `redact_bearer` for why the offset must be
    // absolute rather than relative to the remaining suffix.
    let mut start = 0;
    loop {
        let rest = &line[start..];
        let search = &lower[start..];
        let Some(at) = search.find(key) else {
            out.push_str(rest);
            return out;
        };
        let absolute = start + at;
        let after = absolute + key.len();
        // The key must stand alone: `tokens` and `tokenizer` are not the field.
        let starts_a_word = absolute == 0
            || !line[..absolute]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let located = if starts_a_word {
            locate_value(line, after)
        } else {
            None
        };
        let Some((value_at, delimiter)) = located else {
            out.push_str(&line[start..after]);
            start = after;
            continue;
        };
        // `Authorization: Basic <base64>` puts the credential one token further along. The field
        // name has already established that this is a credential, so the scan skips the scheme
        // word and replaces what follows it — otherwise it would replace `Basic` and leave the
        // secret in place. The scheme word itself stays in the log. A quoted value is replaced
        // whole, scheme and all.
        let value_at = match delimiter {
            Delimiter::Bare => value_at + scheme_prefix_len(&line[value_at..]),
            Delimiter::Quoted(_) => value_at,
        };
        let end = value_len(&line[value_at..], delimiter);
        out.push_str(&line[start..value_at]);
        out.push_str("***");
        start = value_at + end;
    }
}

/// Startup URL of the supervised CLI.
///
/// Documented shape: `dsh web: http://127.0.0.1:59753/?token=xxx[ (LAN: http://10.0.0.5:...)]`.
/// The prefix is what upstream prints today, so the parser must not depend on its exact wording:
/// when it is missing, any token on the line that looks like a loopback startup URL is accepted.
/// Everything else (scheme, host, the token query) is still validated, and the LAN URL upstream
/// appends is rejected on purpose because its host is not loopback.
///
/// `expected_port` guards the prefix-less fallback. Without it any loopback URL with a query
/// would do — a plugin or MCP server printing its own local address during startup would be
/// mistaken for the Harness, and `state.json` would record the wrong port. The prefixed form
/// is the CLI speaking about itself, so it is taken as printed.
pub fn parse_dsh_url(line: &str, expected_port: u16) -> Option<Url> {
    if let Some((_, rest)) = line.split_once(URL_PREFIX) {
        // Only the first whitespace-delimited token: a LAN suffix may follow.
        if let Some(url) = rest.split_whitespace().next().and_then(startup_url) {
            return Some(url);
        }
    }
    line.split_whitespace().find_map(|raw| {
        let url = startup_url(raw)?;
        match url.port() {
            Some(port) if port == expected_port => Some(url),
            // A loopback URL on another port is somebody else's server, not this launch.
            _ => None,
        }
    })
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

/// Bytes kept from `netstat -ano`, which lists every connection on the machine.
#[cfg(windows)]
const NETSTAT_LIMIT: usize = 4 * 1024 * 1024;

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
        // Bounded like every other helper call: `netstat -ano` walks the whole connection
        // table, and the caller is on the startup or takeover path.
        let out = crate::process::output_within(
            std::process::Command::new("netstat").args(["-ano"]),
            PS_TIMEOUT,
            NETSTAT_LIMIT,
        )?;
        parse_netstat_listener(&out.text, port)
    }
}

#[cfg(unix)]
fn listener_pid_lsof(port: u16) -> Option<u32> {
    // `-b` keeps lsof from blocking on a stat it cannot finish (an unresponsive network mount);
    // the budget covers the rest, since this walks every process's descriptors.
    let out = crate::process::stdout_within(
        std::process::Command::new("lsof").args([
            "-b",
            "-nP",
            "-t",
            &format!("-iTCP:{port}"),
            "-sTCP:LISTEN",
        ]),
        PS_TIMEOUT,
    )?;
    out.trim().parse().ok()
}

/// `ss -ltnpH 'sport = :PORT'` line, e.g.
/// `LISTEN 0 511 127.0.0.1:3080 0.0.0.0:* users:(("node",pid=73596,fd=17))`.
#[cfg(target_os = "linux")]
fn listener_pid_ss(port: u16) -> Option<u32> {
    let out = crate::process::stdout_within(
        std::process::Command::new("ss").args(["-ltnpH", &format!("sport = :{port}")]),
        PS_TIMEOUT,
    )?;
    parse_ss_pid(&out)
}

/// The pid listening on `port` in `netstat -ano` output. Compiled everywhere so tests cover it on
/// macOS too.
///
/// Columns are `Proto  Local Address  Foreign Address  State  PID`, and the state word is
/// **localized** (a German Windows prints `ABHÖREN`), so it is not consulted. What identifies a
/// listening socket without it is language-independent: the protocol is TCP, the *local* address
/// ends in `:port`, and the foreign address has port 0 (`0.0.0.0:0`, `[::]:0`). Matching `:port `
/// anywhere on the line also matched outbound connections to a remote server on the same port —
/// and `netstat` sorts by local address, so a `10.x` client socket came before the `127.0.0.1`
/// listener and its pid was returned.
#[allow(dead_code)]
pub fn parse_netstat_listener(output: &str, port: u16) -> Option<u32> {
    let local_suffix = format!(":{port}");
    output.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (proto, local, foreign, pid) = match fields.as_slice() {
            // Four or more fields; the state column, when present, sits between these.
            [proto, local, foreign, .., pid] => (proto, local, foreign, pid),
            _ => return None,
        };
        let listening = proto.eq_ignore_ascii_case("tcp")
            && local.ends_with(&local_suffix)
            && foreign.ends_with(":0");
        if listening {
            pid.parse().ok()
        } else {
            None
        }
    })
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
        forward_lines(stdout, tx.clone(), ring.clone(), log.clone(), opts.port);
    }
    if let Some(stderr) = child.stderr.take() {
        forward_lines(stderr, tx, ring.clone(), log, opts.port);
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
    // The port this launch asked for: the prefix-less URL fallback only accepts this one.
    port: u16,
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
            if let Some(url) = parse_dsh_url(&line, port) {
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
///
/// Redaction happens before the lock is taken: it is the one step here that can panic on hostile
/// input, and a panic while holding `APP_LOGGER` would poison the mutex so that *every* later
/// `app_log` — including the two in `shutdown` — panicked as well. The lock is also taken
/// poison-tolerantly for the same reason: a poisoned logger must still be writable, because the
/// lines it carries are how a failed exit becomes visible at all.
pub fn app_log(line: &str) {
    let safe = format!("[dsh-desktop] {}", redact(line));
    if let Some(logger) = lock_logger() {
        logger.write(&safe);
    }
}

/// The shared app logger, ignoring a poisoned lock rather than propagating the panic.
fn lock_logger() -> Option<Logger> {
    match APP_LOGGER.lock() {
        Ok(logger) => logger.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
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
