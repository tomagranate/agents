//! The preview daemon. Caddy sends `preview.tomagranate.com` and
//! `*.preview.tomagranate.com` here with upstream keep-alive off, so each
//! connection carries one request or one long-lived stream.
//!
//! The index host is answered here. A preview host is proxied at the TCP
//! level: read the request head, pick the port from the host, forward the
//! bytes already read, then pipe both directions until one side closes.
//! WebSockets, HMR, and Server-Sent Events pass through unchanged.

use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{
    service::{self, Activity},
    state::{DOMAIN, Record, StopReason, Store},
};

const INDEX_HTML: &str = include_str!("index.html");
const MAX_HEAD: usize = 64 * 1024;
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// How often `last_used_at` is written for a busy preview.
const TOUCH_INTERVAL: Duration = Duration::from_secs(60);
/// Stopped records older than this are dropped.
const RETENTION: Duration = Duration::from_secs(30 * 86_400);

pub fn serve(store: Store, listen: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(listen)?;
    println!("listening on http://{}", listener.local_addr()?);
    let daemon = Arc::new(Daemon {
        store,
        lock: Mutex::new(()),
    });

    let sweeper = Arc::clone(&daemon);
    thread::spawn(move || {
        loop {
            thread::sleep(SWEEP_INTERVAL);
            if let Err(error) = sweeper.sweep() {
                eprintln!("sweep: {error:#}");
            }
        }
    });

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("accept: {error}");
                continue;
            }
        };
        let daemon = Arc::clone(&daemon);
        thread::spawn(move || {
            if let Err(error) = daemon.handle(stream)
                && !is_disconnect(&error)
            {
                eprintln!("connection: {error:#}");
            }
        });
    }
    Ok(())
}

/// A client that went away mid-response is not worth a log line.
fn is_disconnect(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
        )
    })
}

struct Daemon {
    store: Store,
    /// Serializes record writes made by the daemon's own threads.
    lock: Mutex<()>,
}

/// A parsed HTTP/1.1 request head plus any bytes read past it.
struct Head {
    method: String,
    path: String,
    host: String,
    raw: Vec<u8>,
}

/// What the sweep should do with one record.
#[derive(Debug, PartialEq, Eq)]
pub enum Sweep {
    MarkExited,
    StopIdle,
    Delete,
}

/// Pure decision for the periodic sweep.
pub fn sweep_action(record: &Record, activity: Activity, now: DateTime<Utc>) -> Option<Sweep> {
    let since = |at: DateTime<Utc>| (now - at).to_std().unwrap_or_default();
    match &record.stopped {
        None => match activity {
            Activity::Inactive => Some(Sweep::MarkExited),
            Activity::Active if since(record.last_used_at) >= record.idle_limit() => {
                Some(Sweep::StopIdle)
            }
            _ => None,
        },
        Some(stopped) => {
            let last = stopped.at.max(record.last_used_at);
            (since(last) >= RETENTION).then_some(Sweep::Delete)
        }
    }
}

impl Daemon {
    fn sweep(&self) -> Result<()> {
        let now = Utc::now();
        for mut record in self.store.all()? {
            let activity = service::activity(&record.unit());
            let _guard = self
                .lock
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match sweep_action(&record, activity, now) {
                Some(Sweep::MarkExited) => {
                    record.mark_stopped(StopReason::Exited);
                    self.store.save(&record)?;
                }
                Some(Sweep::StopIdle) => {
                    println!("{}: idle for {}, stopping", record.name, record.idle_ttl);
                    service::stop(&self.store, &mut record, StopReason::Idle)?;
                }
                Some(Sweep::Delete) => {
                    println!("{}: stopped for 30 days, removing", record.name);
                    self.store.remove(&record.name)?;
                }
                None => {}
            }
        }
        Ok(())
    }

    fn handle(&self, mut stream: TcpStream) -> Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let head = match read_head(&mut stream) {
            Ok(head) => head,
            Err(error) => {
                respond(
                    &mut stream,
                    400,
                    "text/plain",
                    format!("{error}").as_bytes(),
                )?;
                return Ok(());
            }
        };
        stream.set_read_timeout(None)?;
        match preview_name(&head.host).map(str::to_owned) {
            Some(name) => self.proxy(stream, head, &name),
            None => self.index(stream, head),
        }
    }

    fn proxy(&self, mut client: TcpStream, head: Head, name: &str) -> Result<()> {
        let Some(record) = self.store.load(name)? else {
            return respond(
                &mut client,
                404,
                "text/html; charset=utf-8",
                notice(name, "There is no preview with this name.").as_bytes(),
            );
        };
        if !record.is_running() {
            return respond(
                &mut client,
                503,
                "text/html; charset=utf-8",
                notice(name, "This preview is stopped. Revive it from the index.").as_bytes(),
            );
        }
        let mut upstream = match TcpStream::connect(("127.0.0.1", record.port)) {
            Ok(upstream) => upstream,
            Err(_) => {
                return respond(
                    &mut client,
                    503,
                    "text/html; charset=utf-8",
                    notice(name, "The preview is not answering on its port yet.").as_bytes(),
                );
            }
        };
        self.touch(record);
        upstream.write_all(&head.raw)?;
        pipe(client, upstream);
        Ok(())
    }

    /// Records use, at most once per `TOUCH_INTERVAL` per preview.
    fn touch(&self, mut record: Record) {
        let now = Utc::now();
        let since = (now - record.last_used_at).to_std().unwrap_or_default();
        if since < TOUCH_INTERVAL {
            return;
        }
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Re-read so a concurrent stop or revive is not overwritten.
        if let Ok(Some(fresh)) = self.store.load(&record.name) {
            record = fresh;
        }
        if record.is_running() {
            record.last_used_at = now;
            if let Err(error) = self.store.save(&record) {
                eprintln!("touch {}: {error:#}", record.name);
            }
        }
    }

    fn index(&self, mut stream: TcpStream, head: Head) -> Result<()> {
        let path = head.path.split('?').next().unwrap_or("/");
        match (head.method.as_str(), path) {
            ("GET", "/") => respond(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                INDEX_HTML.as_bytes(),
            ),
            ("GET", "/api/previews") => {
                let previews: Vec<Summary> =
                    self.store.all()?.into_iter().map(Summary::from).collect();
                json(
                    &mut stream,
                    200,
                    &serde_json::json!({ "previews": previews }),
                )
            }
            ("POST", path) => {
                let Some(rest) = path.strip_prefix("/api/previews/") else {
                    return respond(&mut stream, 404, "text/plain", b"not found");
                };
                let Some((name, action)) = rest.split_once('/') else {
                    return respond(&mut stream, 404, "text/plain", b"not found");
                };
                match self.action(name, action) {
                    Ok(Some(record)) => json(&mut stream, 200, &Summary::from(record)),
                    Ok(None) => json(&mut stream, 200, &serde_json::json!({ "removed": name })),
                    Err(error) => json(
                        &mut stream,
                        status_for(&error),
                        &serde_json::json!({ "error": format!("{error:#}") }),
                    ),
                }
            }
            _ => respond(&mut stream, 404, "text/plain", b"not found"),
        }
    }

    /// Runs one index action. `Ok(None)` means the record is gone.
    fn action(&self, name: &str, action: &str) -> Result<Option<Record>> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut record = self.store.require(name)?;
        match action {
            "revive" => {
                service::start(&self.store, &mut record)?;
                Ok(Some(record))
            }
            "stop" => {
                service::stop(&self.store, &mut record, StopReason::Stopped)?;
                Ok(Some(record))
            }
            "remove" => {
                if record.is_running() {
                    let _ = service::systemctl(&["stop", &record.unit()]);
                }
                self.store.remove(name)?;
                Ok(None)
            }
            _ => bail!("unknown action '{action}'"),
        }
    }
}

fn status_for(error: &anyhow::Error) -> u16 {
    let text = error.to_string();
    if text.contains("not registered") {
        404
    } else if text.contains("unknown action") {
        400
    } else {
        500
    }
}

/// The record as the index sees it.
#[derive(Serialize)]
struct Summary {
    url: String,
    active: bool,
    #[serde(flatten)]
    record: Record,
}

impl From<Record> for Summary {
    fn from(record: Record) -> Self {
        Self {
            url: record.url(),
            active: record.is_running(),
            record,
        }
    }
}

/// `crm.preview.tomagranate.com` gives `crm`. Anything else is the index.
fn preview_name(host: &str) -> Option<&str> {
    let host = host.rsplit_once(':').map_or(host, |(name, _)| name);
    let name = host.strip_suffix(DOMAIN)?.strip_suffix('.')?;
    (!name.is_empty() && !name.contains('.')).then_some(name)
}

fn read_head(stream: &mut TcpStream) -> Result<Head> {
    let mut raw = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(end) = find_head_end(&raw) {
            break end;
        }
        if raw.len() > MAX_HEAD {
            bail!("request head too large");
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            bail!("connection closed before the request head ended");
        }
        raw.extend_from_slice(&chunk[..read]);
    };
    let text = String::from_utf8_lossy(&raw[..end]);
    let mut lines = text.lines();
    let request = lines.next().ok_or_else(|| anyhow!("empty request"))?;
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or("/").to_owned();
    let host = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_default();
    Ok(Head {
        method,
        path,
        host,
        raw,
    })
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

/// Copies bytes both ways until either side closes.
fn pipe(client: TcpStream, upstream: TcpStream) {
    let (mut client_read, mut client_write) = match (client.try_clone(), client) {
        (Ok(read), write) => (read, write),
        (Err(_), _) => return,
    };
    let (mut upstream_read, mut upstream_write) = match (upstream.try_clone(), upstream) {
        (Ok(read), write) => (read, write),
        (Err(_), _) => return,
    };
    let to_upstream = thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Both);
    let _ = upstream_read.shutdown(Shutdown::Both);
    let _ = to_upstream.join();
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn json<T: Serialize>(stream: &mut TcpStream, status: u16, value: &T) -> Result<()> {
    respond(
        stream,
        status,
        "application/json",
        &serde_json::to_vec(value)?,
    )
}

/// A small page for a preview host that cannot be served.
fn notice(name: &str, message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{name} preview</title><style>:root{{color-scheme:light dark}}body{{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1.25rem;line-height:1.6}}</style></head><body><h1>{name}</h1><p>{message}</p><p><a href=\"https://{DOMAIN}/\">Open the preview index</a></p></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use std::path::PathBuf;

    fn record(last_used_minutes_ago: i64) -> Record {
        let now = Utc::now();
        Record {
            name: "demo".to_owned(),
            port: 1,
            cwd: PathBuf::from("/tmp"),
            command: vec!["true".to_owned()],
            path: String::new(),
            idle_ttl: "12h".to_owned(),
            created_at: now,
            started_at: now,
            last_used_at: now - TimeDelta::minutes(last_used_minutes_ago),
            stopped: None,
            last_error: None,
        }
    }

    #[test]
    fn maps_hosts_to_preview_names() {
        assert_eq!(preview_name("crm.preview.tomagranate.com"), Some("crm"));
        assert_eq!(preview_name("crm.preview.tomagranate.com:443"), Some("crm"));
        assert_eq!(preview_name("preview.tomagranate.com"), None);
        assert_eq!(preview_name("a.b.preview.tomagranate.com"), None);
        assert_eq!(preview_name("127.0.0.1:8770"), None);
        assert_eq!(preview_name(""), None);
    }

    #[test]
    fn sweeps_by_activity_and_age() {
        let now = Utc::now();
        assert_eq!(sweep_action(&record(5), Activity::Active, now), None);
        assert_eq!(sweep_action(&record(5), Activity::Unknown, now), None);
        assert_eq!(
            sweep_action(&record(5), Activity::Inactive, now),
            Some(Sweep::MarkExited)
        );
        assert_eq!(
            sweep_action(&record(13 * 60), Activity::Active, now),
            Some(Sweep::StopIdle)
        );

        let mut stopped = record(13 * 60);
        stopped.mark_stopped(StopReason::Idle);
        assert_eq!(sweep_action(&stopped, Activity::Inactive, now), None);
        stopped.stopped.as_mut().unwrap().at = now - TimeDelta::days(31);
        stopped.last_used_at = now - TimeDelta::days(31);
        assert_eq!(
            sweep_action(&stopped, Activity::Inactive, now),
            Some(Sweep::Delete)
        );
    }
}
