//! Spawn `dsh web`, stream its output, parse the startup URL, and probe for a live instance.

use crate::locator::DshLocation;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

pub const URL_PREFIX: &str = "dsh web:";
const RING_CAPACITY: usize = 200;
const LOG_LIMIT_BYTES: u64 = 5 * 1024 * 1024;

/// Last N lines of Harness output, with the launch token redacted.
#[derive(Clone)]
pub struct Ring(Arc<Mutex<VecDeque<String>>>);

impl Default for Ring {
    fn default() -> Self {
        Ring::new()
    }
}

impl Ring {
    pub fn new() -> Self {
        Ring(Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY))))
    }
    pub fn push(&self, line: &str) {
        let mut q = self.0.lock().unwrap();
        if q.len() == RING_CAPACITY {
            q.pop_front();
        }
        q.push_back(redact(line));
    }
    pub fn tail(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Replace `token=...` with `token=***` so credentials never reach the log file.
pub fn redact(line: &str) -> String {
    match line.find("token=") {
        None => line.to_string(),
        Some(idx) => {
            let head = &line[..idx + "token=".len()];
            let rest = &line[idx + "token=".len()..];
            let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            format!("{head}***{}", &rest[end..])
        }
    }
}

/// `dsh web: http://127.0.0.1:59753/?token=xxx[ (LAN: http://10.0.0.5:...)]`
pub fn parse_dsh_url(line: &str) -> Option<Url> {
    let rest = line.split_once(URL_PREFIX)?.1;
    // Take only the first whitespace-delimited token: a LAN suffix may follow.
    let raw = rest.split_whitespace().next()?;
    let url = Url::parse(raw).ok()?;
    if url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.query().is_some() {
        Some(url)
    } else {
        None
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    /// A Harness answered but this process holds no session cookie.
    HarnessNoSession,
    /// A Harness answered and accepted the request.
    HarnessWithSession,
    /// Something else owns the port.
    Other,
    /// Nothing is listening.
    Closed,
}

/// Bound every network touch: a peer that accepts the connection and then stays silent
/// must not be able to park the startup thread forever.
const PROBE_TIMEOUT: Duration = Duration::from_millis(600);

/// Dependency-free HTTP probe over loopback: the auth fence identifies a Harness.
pub fn probe(port: u16) -> Probe {
    let addr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return Probe::Other,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return Probe::Closed,
    };
    // Connect timeouts do not cover the exchange: without these the read below can block
    // until the peer decides to answer, which it may never do.
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let request = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: dsh-desktop\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return Probe::Other;
    }
    let mut body = String::new();
    let _ = stream.read_to_string(&mut body);
    if body.starts_with("HTTP/1.1 401") || body.starts_with("HTTP/1.0 401") {
        if body.contains("dsh web authentication required") {
            Probe::HarnessNoSession
        } else {
            Probe::Other
        }
    } else if body.starts_with("HTTP/1.1 200") || body.starts_with("HTTP/1.0 200") {
        Probe::HarnessWithSession
    } else {
        Probe::Other
    }
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
pub fn spawn(loc: &DshLocation, opts: &SpawnOptions<'_>) -> std::io::Result<Spawned> {
    let mut command = Command::new(&loc.node);
    command
        .arg(&loc.dsh_js)
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
}

fn forward_lines<R: Read + Send + 'static>(
    reader: R,
    tx: mpsc::Sender<Url>,
    ring: Ring,
    log: Logger,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            ring.push(&line);
            log.write(&redact(&line));
            if let Some(url) = parse_dsh_url(&line) {
                let _ = tx.send(url);
            }
        }
    });
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

#[derive(Clone)]
pub struct Logger {
    path: PathBuf,
    file: Arc<Mutex<Option<File>>>,
}

impl Logger {
    pub fn open(path: &Path) -> Self {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(path)
            .map(|m| m.len() > LOG_LIMIT_BYTES)
            .unwrap_or(false)
        {
            let rotated = path.with_extension("log.1");
            let _ = std::fs::rename(path, rotated);
        }
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        Logger {
            path: path.to_path_buf(),
            file: Arc::new(Mutex::new(file)),
        }
    }

    pub fn write(&self, line: &str) {
        let mut guard = self.file.lock().unwrap();
        if guard.is_none() {
            *guard = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok();
        }
        if let Some(file) = guard.as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_url() {
        let url = parse_dsh_url("dsh web: http://127.0.0.1:59753/?token=abc").unwrap();
        assert_eq!(url.port(), Some(59753));
    }

    #[test]
    fn ignores_lan_suffix() {
        let line =
            "dsh web: http://127.0.0.1:59753/?token=abc (LAN: http://10.0.0.5:59753/?token=abc)";
        let url = parse_dsh_url(line).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(59753));
    }

    #[test]
    fn rejects_foreign_hosts_and_plain_lines() {
        assert!(parse_dsh_url("dsh web: http://example.com/?token=abc").is_none());
        assert!(parse_dsh_url("dsh web: listening").is_none());
        assert!(parse_dsh_url("hello").is_none());
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
}
