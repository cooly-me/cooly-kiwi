//! Audit sinks: where audit records go besides the local log.
//!
//! The hash-chained local log stays the source of truth; a sink ships a copy of each
//! record (the same JSON line, hash and chain link included, so a receiver can verify
//! the chain and keep the latest hash as a truncation anchor).
//!
//! Every sink sits behind its own bounded queue and worker thread. Handing a record to
//! a sink never blocks and never fails the answer: when a sink is down and its queue is
//! full, records for it are dropped and counted (the receiver sees the gap in `seq`).
//! When the server stops, each worker gets a short grace period to flush.
//!
//! Kinds: `stdout` and `stderr` (JSON lines), `http` (batches of JSON lines POSTed as
//! `application/x-ndjson`, retried with backoff), `syslog` (RFC 5424 over UDP or a
//! Unix datagram socket).

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use kiwi_core::plugin::AuditSink;
use kiwi_http::{Client, Endpoint, Policy, secret_from_env};
use serde::{Deserialize, Serialize};

/// How long a stopping server waits for each sink to flush.
const FLUSH_ON_STOP: Duration = Duration::from_secs(3);

/// One sink in `kiwi.toml` (`[[audit.sinks]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkConfig {
    /// `stdout`, `stderr`, `http` or `syslog`.
    pub kind: String,
    /// `http`: the endpoint (https, or http on a loopback address).
    #[serde(default)]
    pub url: Option<String>,
    /// `http`: the environment variable holding a bearer token.
    #[serde(default)]
    pub bearer_env: Option<String>,
    /// `http`: records per request.
    #[serde(default = "defaults::batch")]
    pub batch: u32,
    /// `http`: the longest a record waits for its batch to fill, in milliseconds.
    #[serde(default = "defaults::flush_ms")]
    pub flush_ms: u64,
    /// Records held while the sink is slow or down; beyond it they are dropped.
    #[serde(default = "defaults::queue")]
    pub queue: u32,
    /// `http`: limit for one request, in milliseconds.
    #[serde(default = "defaults::timeout_ms")]
    pub timeout_ms: u64,
    /// `http`: retries of a failed batch.
    #[serde(default = "defaults::retries")]
    pub retries: u32,
    /// `syslog`: `udp://host:port` or `unix:///dev/log`.
    #[serde(default)]
    pub address: Option<String>,
    /// `syslog`: the facility, `local0` (default) to `local7`, `user` or `auth`.
    #[serde(default)]
    pub facility: Option<String>,
}

mod defaults {
    pub(super) fn batch() -> u32 {
        100
    }
    pub(super) fn flush_ms() -> u64 {
        1_000
    }
    pub(super) fn queue() -> u32 {
        10_000
    }
    pub(super) fn timeout_ms() -> u64 {
        5_000
    }
    pub(super) fn retries() -> u32 {
        5
    }
}

fn facility_code(name: &str) -> Option<u8> {
    match name {
        "user" => Some(1),
        "auth" => Some(4),
        "local0" => Some(16),
        "local1" => Some(17),
        "local2" => Some(18),
        "local3" => Some(19),
        "local4" => Some(20),
        "local5" => Some(21),
        "local6" => Some(22),
        "local7" => Some(23),
        _ => None,
    }
}

impl SinkConfig {
    /// A sink of `kind` with defaults.
    pub fn new(kind: &str) -> Self {
        Self {
            kind: kind.to_owned(),
            url: None,
            bearer_env: None,
            batch: defaults::batch(),
            flush_ms: defaults::flush_ms(),
            queue: defaults::queue(),
            timeout_ms: defaults::timeout_ms(),
            retries: defaults::retries(),
            address: None,
            facility: None,
        }
    }

    /// Checks the settings for the sink's kind.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=1_000_000).contains(&self.queue) {
            return Err("queue must be between 1 and 1000000".into());
        }
        let http_keys = self.url.is_some() || self.bearer_env.is_some();
        let syslog_keys = self.address.is_some() || self.facility.is_some();
        match self.kind.as_str() {
            "stdout" | "stderr" => {
                if http_keys || syslog_keys {
                    return Err(format!("a {} sink takes no url or address", self.kind));
                }
            }
            "http" => {
                if syslog_keys {
                    return Err("address and facility apply to syslog sinks".into());
                }
                let url = self.url.as_deref().ok_or("an http sink needs url")?;
                Endpoint::parse(url).map_err(|e| e.to_string())?;
                if let Some(var) = &self.bearer_env
                    && (var.is_empty()
                        || !var
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                {
                    return Err("bearer_env must name an environment variable, not hold a secret".into());
                }
                if !(1..=10_000).contains(&self.batch) {
                    return Err("batch must be between 1 and 10000".into());
                }
                if !(10..=600_000).contains(&self.flush_ms) {
                    return Err("flush_ms must be between 10 and 600000".into());
                }
                if !(100..=600_000).contains(&self.timeout_ms) {
                    return Err("timeout_ms must be between 100 and 600000".into());
                }
                if self.retries > 20 {
                    return Err("retries must be at most 20".into());
                }
            }
            "syslog" => {
                if http_keys {
                    return Err("url and bearer_env apply to http sinks".into());
                }
                let address = self.address.as_deref().ok_or("a syslog sink needs address")?;
                SyslogTarget::parse(address)?;
                if let Some(facility) = &self.facility
                    && facility_code(facility).is_none()
                {
                    return Err(format!(
                        "facility `{facility}` is not local0-local7, user or auth"
                    ));
                }
            }
            other => return Err(format!("kind `{other}` is not stdout, stderr, http or syslog")),
        }
        Ok(())
    }
}

/// Builds the sinks of a configuration. `stdout_carries_protocol` refuses a `stdout`
/// sink when standard output is the MCP transport (stdio), where it would corrupt it.
pub fn build(
    configs: &[SinkConfig],
    stdout_carries_protocol: bool,
) -> Result<Vec<Box<dyn AuditSink>>, String> {
    let mut sinks: Vec<Box<dyn AuditSink>> = Vec::new();
    for (index, config) in configs.iter().enumerate() {
        let context = |e: String| format!("audit.sinks[{index}] ({}): {e}", config.kind);
        config.validate().map_err(context)?;
        let queue = config.queue as usize;
        let sink = match config.kind.as_str() {
            "stdout" if stdout_carries_protocol => {
                return Err(context(
                    "standard output carries the MCP protocol over stdio; use a stderr sink".into(),
                ));
            }
            "stdout" => Queued::spawn("stdout", queue, Lines::new(Stream::Stdout)),
            "stderr" => Queued::spawn("stderr", queue, Lines::new(Stream::Stderr)),
            "http" => Queued::spawn("http", queue, HttpBatches::new(config).map_err(context)?),
            "syslog" => Queued::spawn("syslog", queue, Syslog::new(config).map_err(context)?),
            other => return Err(context(format!("unknown kind `{other}`"))),
        };
        sinks.push(Box::new(sink.map_err(|e| context(e.to_string()))?));
    }
    Ok(sinks)
}

/// The work behind a queue: what a worker does with the records it receives.
trait Worker: Send + 'static {
    /// Runs until the queue closes; `dropped` counts records it had to give up.
    fn run(self: Box<Self>, queue: Receiver<String>, dropped: &AtomicU64);
}

/// A sink in front of a worker thread, through a bounded queue.
struct Queued {
    name: &'static str,
    tx: std::sync::Mutex<Option<SyncSender<String>>>,
    dropped: Arc<AtomicU64>,
    done: std::sync::Mutex<Receiver<()>>,
}

impl Queued {
    fn spawn(name: &'static str, queue: usize, worker: impl Worker) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(queue);
        let (done_tx, done) = mpsc::channel();
        let dropped = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&dropped);
        let worker: Box<dyn Worker> = Box::new(worker);
        std::thread::Builder::new()
            .name(format!("kiwi-audit-{name}"))
            .spawn(move || {
                worker.run(rx, &counter);
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            name,
            tx: std::sync::Mutex::new(Some(tx)),
            dropped,
            done: std::sync::Mutex::new(done),
        })
    }
}

/// Counts a dropped record, warning at 1, 2, 4, 8… so a long outage does not flood the
/// log.
fn note_drop(sink: &str, dropped: &AtomicU64, records: u64) {
    let before = dropped.fetch_add(records, Ordering::Relaxed);
    let after = before + records;
    if before == 0 || (after.ilog2() != before.ilog2()) {
        tracing::warn!(sink, dropped = after, "audit sink is behind; records dropped");
    }
}

impl AuditSink for Queued {
    fn name(&self) -> &str {
        self.name
    }

    fn deliver(&self, line: &str) -> kiwi_core::Result<()> {
        let sent = match self.tx.lock() {
            Ok(tx) => match tx.as_ref() {
                Some(tx) => match tx.try_send(line.to_owned()) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
                },
                None => false,
            },
            Err(_) => false,
        };
        if !sent {
            note_drop(self.name, &self.dropped, 1);
        }
        Ok(())
    }

    fn close(&self) {
        // Closing the queue lets the worker drain what is left and stop.
        let open = self.tx.lock().ok().and_then(|mut tx| tx.take()).is_some();
        if !open {
            return;
        }
        let flushed = self
            .done
            .lock()
            .map(|done| done.recv_timeout(FLUSH_ON_STOP).is_ok())
            .unwrap_or(false);
        if !flushed {
            tracing::warn!(
                sink = self.name,
                "audit sink did not flush before the server stopped"
            );
        }
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            tracing::warn!(
                sink = self.name,
                dropped,
                "audit records were not delivered to this sink"
            );
        }
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.close();
    }
}

enum Stream {
    Stdout,
    Stderr,
}

/// JSON lines on standard output or standard error.
struct Lines {
    stream: Stream,
}

impl Lines {
    fn new(stream: Stream) -> Self {
        Self { stream }
    }
}

impl Worker for Lines {
    fn run(self: Box<Self>, queue: Receiver<String>, dropped: &AtomicU64) {
        while let Ok(first) = queue.recv() {
            let mut text = first;
            text.push('\n');
            let mut count = 1u64;
            while let Ok(more) = queue.try_recv() {
                text.push_str(&more);
                text.push('\n');
                count += 1;
            }
            let written = match self.stream {
                Stream::Stdout => {
                    let mut out = std::io::stdout().lock();
                    out.write_all(text.as_bytes()).and_then(|()| out.flush())
                }
                Stream::Stderr => {
                    let mut out = std::io::stderr().lock();
                    out.write_all(text.as_bytes()).and_then(|()| out.flush())
                }
            };
            if written.is_err() {
                note_drop("lines", dropped, count);
            }
        }
    }
}

/// Batches POSTed to an HTTP endpoint.
struct HttpBatches {
    endpoint: Endpoint,
    client: Client,
    token: Option<String>,
    batch: usize,
    flush: Duration,
}

impl HttpBatches {
    fn new(config: &SinkConfig) -> Result<Self, String> {
        let url = config.url.as_deref().ok_or("an http sink needs url")?;
        let endpoint = Endpoint::parse(url).map_err(|e| e.to_string())?;
        let token = match &config.bearer_env {
            Some(var) => Some(secret_from_env(var).map_err(|e| e.to_string())?),
            None => None,
        };
        let timeout = Duration::from_millis(config.timeout_ms);
        let client = Client::new(Policy {
            timeout,
            retries: config.retries,
            backoff: Duration::from_millis(250),
            deadline: timeout.saturating_mul(config.retries + 1) + Duration::from_secs(120),
        });
        Ok(Self {
            endpoint,
            client,
            token,
            batch: config.batch as usize,
            flush: Duration::from_millis(config.flush_ms),
        })
    }

    fn send(&self, lines: &[String], dropped: &AtomicU64) {
        let mut body = lines.join("\n");
        body.push('\n');
        if let Err(error) = self.client.post(
            &self.endpoint,
            "application/x-ndjson",
            body.as_bytes(),
            self.token.as_deref(),
        ) {
            tracing::warn!(sink = "http", %error, records = lines.len(), "audit batch not delivered");
            note_drop("http", dropped, lines.len() as u64);
        }
    }
}

impl Worker for HttpBatches {
    fn run(self: Box<Self>, queue: Receiver<String>, dropped: &AtomicU64) {
        let mut pending: Vec<String> = Vec::with_capacity(self.batch);
        let mut deadline: Option<Instant> = None;
        loop {
            let wait = deadline.map_or(Duration::from_secs(3600), |d| {
                d.saturating_duration_since(Instant::now())
            });
            match queue.recv_timeout(wait) {
                Ok(line) => {
                    if pending.is_empty() {
                        deadline = Some(Instant::now() + self.flush);
                    }
                    pending.push(line);
                    if pending.len() >= self.batch {
                        self.send(&pending, dropped);
                        pending.clear();
                        deadline = None;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if !pending.is_empty() {
                        self.send(&pending, dropped);
                        pending.clear();
                    }
                    deadline = None;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    if !pending.is_empty() {
                        self.send(&pending, dropped);
                    }
                    return;
                }
            }
        }
    }
}

/// Where syslog datagrams go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SyslogTarget {
    Udp(String),
    #[cfg(unix)]
    Unix(std::path::PathBuf),
}

impl SyslogTarget {
    fn parse(address: &str) -> Result<Self, String> {
        if let Some(rest) = address.strip_prefix("udp://") {
            if rest
                .rsplit_once(':')
                .is_none_or(|(_, port)| port.parse::<u16>().is_err())
            {
                return Err(format!("`{address}` needs a port (udp://host:514)"));
            }
            return Ok(Self::Udp(rest.to_owned()));
        }
        #[cfg(unix)]
        if let Some(path) = address.strip_prefix("unix://") {
            return Ok(Self::Unix(path.into()));
        }
        Err(format!("`{address}` is not udp://host:port or unix:///path"))
    }
}

/// RFC 5424 messages over UDP or a Unix datagram socket.
struct Syslog {
    target: SyslogTarget,
    pri: u8,
    host: String,
}

impl Syslog {
    fn new(config: &SinkConfig) -> Result<Self, String> {
        let address = config.address.as_deref().ok_or("a syslog sink needs address")?;
        let facility = config.facility.as_deref().unwrap_or("local0");
        let code = facility_code(facility).ok_or_else(|| format!("unknown facility `{facility}`"))?;
        let host = std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.is_empty() && h.is_ascii() && !h.contains(' '))
            .unwrap_or_else(|| "-".to_owned());
        Ok(Self {
            target: SyslogTarget::parse(address)?,
            // Severity 6: informational.
            pri: code * 8 + 6,
            host,
        })
    }

    fn message(&self, line: &str) -> String {
        format!(
            "<{}>1 {} {} kiwi {} kiwi-audit - {line}",
            self.pri,
            kiwi_core::date::timestamp(kiwi_core::date::now_unix()),
            self.host,
            std::process::id()
        )
    }
}

impl Worker for Syslog {
    fn run(self: Box<Self>, queue: Receiver<String>, dropped: &AtomicU64) {
        match &self.target {
            SyslogTarget::Udp(addr) => {
                // A socket of the target's address family (IPv4 or IPv6).
                use std::net::ToSocketAddrs;
                let socket = addr
                    .as_str()
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut addrs| addrs.next())
                    .and_then(|target| {
                        let local = if target.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
                        std::net::UdpSocket::bind(local)
                            .ok()
                            .map(|socket| (socket, target))
                    });
                let Some((socket, target)) = socket else {
                    tracing::warn!(sink = "syslog", %addr, "cannot resolve the syslog address or open a socket");
                    while queue.recv().is_ok() {
                        note_drop("syslog", dropped, 1);
                    }
                    return;
                };
                while let Ok(line) = queue.recv() {
                    if socket.send_to(self.message(&line).as_bytes(), target).is_err() {
                        note_drop("syslog", dropped, 1);
                    }
                }
            }
            #[cfg(unix)]
            SyslogTarget::Unix(path) => {
                let socket = std::os::unix::net::UnixDatagram::unbound();
                let Ok(socket) = socket else {
                    while queue.recv().is_ok() {
                        note_drop("syslog", dropped, 1);
                    }
                    return;
                };
                while let Ok(line) = queue.recv() {
                    if socket.send_to(self.message(&line).as_bytes(), path).is_err() {
                        note_drop("syslog", dropped, 1);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::io::{BufRead, BufReader, Read};
    use std::net::{TcpListener, UdpSocket};
    use std::sync::Mutex;

    use super::*;

    fn http_config(url: String) -> SinkConfig {
        SinkConfig {
            url: Some(url),
            batch: 3,
            flush_ms: 50,
            timeout_ms: 500,
            retries: 2,
            ..SinkConfig::new("http")
        }
    }

    /// Records each request body; answers the first `fail` with a 503.
    fn collector(fail: usize) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&bodies);
        std::thread::spawn(move || {
            let mut count = 0;
            while let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                count += 1;
                let mut stream = reader.into_inner();
                if count <= fail {
                    let _ = stream.write_all(
                        b"HTTP/1.1 503 X\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    );
                    continue;
                }
                log.lock().unwrap().push(String::from_utf8(body).unwrap());
                let _ =
                    stream.write_all(b"HTTP/1.1 204 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            }
        });
        (format!("http://{addr}/audit"), bodies)
    }

    #[test]
    fn http_batches_and_retries() {
        let (url, bodies) = collector(1);
        let sinks = build(&[http_config(url)], false).unwrap();
        for n in 0..7 {
            sinks[0].deliver(&format!("{{\"seq\":{n}}}")).unwrap();
        }
        drop(sinks);
        let bodies = bodies.lock().unwrap();
        let lines: Vec<&str> = bodies.iter().flat_map(|b| b.lines()).collect();
        assert_eq!(lines.len(), 7, "{bodies:?}");
        assert_eq!(lines[0], "{\"seq\":0}");
        assert!(bodies.iter().all(|b| b.lines().count() <= 3));
    }

    #[test]
    fn a_dead_endpoint_never_blocks_delivery() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut config = http_config(format!("http://127.0.0.1:{port}/audit"));
        config.queue = 2;
        config.retries = 0;
        let sinks = build(&[config], false).unwrap();
        let started = Instant::now();
        for n in 0..1_000 {
            sinks[0].deliver(&format!("{{\"seq\":{n}}}")).unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn syslog_over_udp() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let config = SinkConfig {
            address: Some(format!("udp://{}", receiver.local_addr().unwrap())),
            facility: Some("local3".into()),
            ..SinkConfig::new("syslog")
        };
        let sinks = build(&[config], false).unwrap();
        sinks[0].deliver("{\"seq\":1}").unwrap();
        let mut buffer = [0u8; 2048];
        let (n, _) = receiver.recv_from(&mut buffer).unwrap();
        let message = String::from_utf8_lossy(&buffer[..n]);
        assert!(message.starts_with("<158>1 "), "{message}");
        assert!(message.ends_with(" kiwi-audit - {\"seq\":1}"), "{message}");
    }

    #[test]
    fn configurations_are_checked() {
        assert!(
            build(&[SinkConfig::new("stdout")], true).is_err(),
            "stdout is the protocol"
        );
        assert!(build(&[SinkConfig::new("stdout")], false).is_ok());
        assert!(SinkConfig::new("stderr").validate().is_ok());
        assert!(SinkConfig::new("http").validate().is_err());
        let plain = SinkConfig {
            url: Some("http://siem.example.com/x".into()),
            ..SinkConfig::new("http")
        };
        assert!(plain.validate().is_err(), "plain http off loopback");
        let secret = SinkConfig {
            url: Some("https://siem.example.com/x".into()),
            bearer_env: Some("Bearer abc123".into()),
            ..SinkConfig::new("http")
        };
        assert!(secret.validate().is_err());
        let syslog = SinkConfig {
            address: Some("udp://host".into()),
            ..SinkConfig::new("syslog")
        };
        assert!(syslog.validate().is_err(), "no port");
        let unset = SinkConfig {
            url: Some("https://siem.example.com/x".into()),
            bearer_env: Some("KIWI_TEST_SINK_TOKEN_UNSET".into()),
            ..SinkConfig::new("http")
        };
        let message = build(&[unset], false).err().unwrap();
        assert!(message.contains("KIWI_TEST_SINK_TOKEN_UNSET"), "{message}");
    }
}
