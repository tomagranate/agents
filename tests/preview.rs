//! Black-box tests for `agents preview serve`: routing by host, proxying,
//! stream pass-through, and the index API. systemd is not involved.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command as StdCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use assert_cmd::cargo::cargo_bin;
use serde_json::json;
use tempfile::TempDir;

const DOMAIN: &str = "preview.tomagranate.com";

struct Daemon {
    child: Child,
    port: u16,
    home: TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn daemon() -> Daemon {
    let home = TempDir::new().unwrap();
    let port = free_port();
    let child = StdCommand::new(cargo_bin("agents"))
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join(".state"))
        .args(["preview", "serve", "--listen", &format!("127.0.0.1:{port}")])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon did not listen"
        );
        thread::sleep(Duration::from_millis(50));
    }
    Daemon { child, port, home }
}

impl Daemon {
    fn records(&self) -> std::path::PathBuf {
        self.home.path().join(".state/agents/previews")
    }

    fn write_record(&self, name: &str, port: u16, stopped: bool) {
        fs::create_dir_all(self.records()).unwrap();
        let mut record = json!({
            "name": name,
            "port": port,
            "cwd": "/tmp",
            "command": ["true"],
            "path": "/usr/bin",
            "idle_ttl": "12h",
            "created_at": "2026-09-01T00:00:00Z",
            "started_at": "2026-09-01T00:00:00Z",
            "last_used_at": "2026-09-01T00:00:00Z"
        });
        if stopped {
            record["stopped"] = json!({ "at": "2026-09-02T00:00:00Z", "reason": "idle" });
        }
        fs::write(
            self.records().join(format!("{name}.json")),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
    }

    /// Sends one raw request and returns the whole response.
    fn request(&self, host: &str, method: &str, path: &str) -> String {
        self.request_with(host, method, path, "")
    }

    /// Like `request`, with extra header lines (each ending in CRLF).
    fn request_with(&self, host: &str, method: &str, path: &str, extra: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
}

/// A stub dev server that answers every request with its name.
fn stub_http(name: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let body = format!("hello from {name}");
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

/// A stub that echoes every byte after the request head, like a WebSocket
/// server would after the upgrade.
fn stub_echo() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let _ = stream.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: echo\r\nConnection: Upgrade\r\n\r\n",
            );
            let mut buffer = [0u8; 1024];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if stream.write_all(&buffer[..read]).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    port
}

#[test]
fn proxies_by_host_and_reports_missing_or_stopped_previews() {
    let daemon = daemon();
    let alpha = stub_http("alpha");
    let beta = stub_http("beta");
    daemon.write_record("alpha", alpha, false);
    daemon.write_record("beta", beta, false);
    daemon.write_record("sleepy", free_port(), true);
    daemon.write_record("dead", free_port(), false);

    let response = daemon.request(&format!("alpha.{DOMAIN}"), "GET", "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("hello from alpha"), "{response}");

    let response = daemon.request(&format!("beta.{DOMAIN}:443"), "GET", "/deep/path?x=1");
    assert!(response.ends_with("hello from beta"), "{response}");

    let response = daemon.request(&format!("nope.{DOMAIN}"), "GET", "/");
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(response.contains("no preview with this name"), "{response}");

    let response = daemon.request(&format!("sleepy.{DOMAIN}"), "GET", "/");
    assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    assert!(response.contains("Revive it from the index"), "{response}");

    let response = daemon.request(&format!("dead.{DOMAIN}"), "GET", "/");
    assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    assert!(response.contains("not answering"), "{response}");
}

#[test]
fn pipes_upgraded_streams_both_ways() {
    let daemon = daemon();
    daemon.write_record("ws", stub_echo(), false);

    let mut stream = TcpStream::connect(("127.0.0.1", daemon.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /socket HTTP/1.1\r\nHost: ws.{DOMAIN}\r\nUpgrade: echo\r\nConnection: Upgrade\r\n\r\n"
    )
    .unwrap();
    let mut buffer = vec![0u8; 4096];
    let mut got = Vec::new();
    while !got.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "upstream closed before the upgrade response");
        got.extend_from_slice(&buffer[..read]);
    }
    assert!(
        got.starts_with(b"HTTP/1.1 101"),
        "{}",
        String::from_utf8_lossy(&got)
    );

    for message in ["first frame", "second frame"] {
        stream.write_all(message.as_bytes()).unwrap();
        let mut echoed = vec![0u8; message.len()];
        stream.read_exact(&mut echoed).unwrap();
        assert_eq!(echoed, message.as_bytes());
    }
}

#[test]
fn serves_the_index_and_api_on_the_bare_domain() {
    let daemon = daemon();
    daemon.write_record("alpha", 1, false);
    daemon.write_record("sleepy", 2, true);

    let page = daemon.request(DOMAIN, "GET", "/");
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("text/html"), "{page}");
    assert!(page.contains("<title>Previews</title>"), "{page}");

    // The API works on any non-preview host, so curl on the box needs no Host header.
    let response = daemon.request(
        &format!("127.0.0.1:{}", daemon.port),
        "GET",
        "/api/previews",
    );
    let body = response.split("\r\n\r\n").nth(1).unwrap();
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    let previews = value["previews"].as_array().unwrap();
    assert_eq!(previews.len(), 2);
    assert_eq!(previews[0]["name"], "alpha");
    assert_eq!(previews[0]["active"], true);
    assert_eq!(previews[0]["url"], format!("https://alpha.{DOMAIN}/"));
    assert_eq!(previews[1]["active"], false);
    assert_eq!(previews[1]["stopped"]["reason"], "idle");

    let response = daemon.request(DOMAIN, "POST", "/api/previews/sleepy/remove");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(!daemon.records().join("sleepy.json").exists());

    let response = daemon.request(DOMAIN, "POST", "/api/previews/missing/revive");
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(response.contains("not registered"), "{response}");

    let response = daemon.request(DOMAIN, "POST", "/api/previews/alpha/dance");
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
}

#[test]
fn refuses_actions_from_other_origins() {
    let daemon = daemon();
    daemon.write_record("alpha", 1, true);

    let response = daemon.request_with(
        DOMAIN,
        "POST",
        "/api/previews/alpha/remove",
        "Origin: https://evil.example\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(daemon.records().join("alpha.json").exists());

    let response = daemon.request_with(
        DOMAIN,
        "POST",
        "/api/previews/alpha/remove",
        &format!("Origin: https://{DOMAIN}\r\n"),
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(!daemon.records().join("alpha.json").exists());
}

#[test]
fn rejects_oversized_request_heads() {
    let daemon = daemon();
    let mut stream = TcpStream::connect(("127.0.0.1", daemon.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "GET / HTTP/1.1\r\nHost: {DOMAIN}\r\n").unwrap();
    let junk = vec![b'a'; 70 * 1024];
    let _ = stream.write_all(&junk);
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
}
