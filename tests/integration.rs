// Black-box integration tests: spawn the real compiled binary on an OS-assigned
// port (PORT=0), then drive it over a raw TCP socket and assert on the wire
// behaviour that apt and portage rely on. No HTTP client dependency; we speak
// just enough HTTP/1.1 by hand and always send `Connection: close` so the server
// closes the socket after each response and reads terminate cleanly.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestServer {
    child: Child,
    port: u16,
    dir: PathBuf,
}

impl TestServer {
    fn start(setup: impl FnOnce(&Path)) -> TestServer {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("fs-it-{}-{}", std::process::id(), n));
        let www = dir.join("www");
        std::fs::create_dir_all(&www).unwrap();
        setup(&www);

        let mut child = Command::new(env!("CARGO_BIN_EXE_file-server"))
            .env("FILE_SERVER_DIR", &www)
            .env("PORT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn server");

        // Banner line: "Serving '<dir>' on http://0.0.0.0:<port>"
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read server banner");
        let port = line
            .trim()
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("could not parse port from banner: {:?}", line));

        // Keep draining stdout so the server's per-request logging never writes
        // into a closed pipe. The thread exits at EOF when the child is killed.
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });

        TestServer { child, port, dir }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn req(port: u16, method: &str, path: &str, extra: &[&str]) -> Resp {
    let mut s = format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n",
        method, path
    );
    for h in extra {
        s.push_str(h);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(s.as_bytes()).unwrap();

    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break
            }
            Err(e) => panic!("socket read error: {}", e),
        }
    }
    parse_response(&buf)
}

fn parse_response(buf: &[u8]) -> Resp {
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator");
    let head = std::str::from_utf8(&buf[..split]).expect("utf8 headers");
    let body = buf[split + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status");
    let headers = lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (k, v) = l.split_once(':').expect("header colon");
            (k.trim().to_string(), v.trim().to_string())
        })
        .collect();

    Resp {
        status,
        headers,
        body,
    }
}

// Deterministic payload so range slices can be checked exactly.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 256) as u8).collect()
}

#[test]
fn full_get_sends_validators_and_content_length() {
    let srv = TestServer::start(|www| {
        std::fs::write(www.join("hello.txt"), b"hello world").unwrap();
    });
    let r = req(srv.port, "GET", "/hello.txt", &[]);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Content-Length"), Some("11"));
    assert_eq!(r.header("Accept-Ranges"), Some("bytes"));
    assert!(r.header("ETag").is_some());
    assert!(r.header("Last-Modified").is_some());
    assert_eq!(r.body, b"hello world");
}

#[test]
fn large_file_keeps_content_length_and_is_not_chunked() {
    // Regression guard: tiny_http's 32 KB default would chunk this and drop
    // Content-Length. with_chunked_threshold(MAX) must keep identity encoding.
    let data = pattern(1_048_576);
    let payload = data.clone();
    let srv = TestServer::start(move |www| {
        std::fs::write(www.join("big.deb"), &payload).unwrap();
    });
    let r = req(srv.port, "GET", "/big.deb", &[]);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Content-Length"), Some("1048576"));
    assert_eq!(r.header("Transfer-Encoding"), None);
    assert_eq!(r.body.len(), 1_048_576);
    assert_eq!(r.body, data);
}

#[test]
fn range_request_returns_206_with_exact_slice() {
    let data = pattern(1_048_576);
    let payload = data.clone();
    let srv = TestServer::start(move |www| {
        std::fs::write(www.join("big.deb"), &payload).unwrap();
    });
    let r = req(srv.port, "GET", "/big.deb", &["Range: bytes=1048000-"]);
    assert_eq!(r.status, 206);
    assert_eq!(r.header("Content-Range"), Some("bytes 1048000-1048575/1048576"));
    assert_eq!(r.header("Content-Length"), Some("576"));
    assert_eq!(r.body, data[1_048_000..]);
}

#[test]
fn suffix_range_returns_last_n_bytes() {
    let data = pattern(1_048_576);
    let payload = data.clone();
    let srv = TestServer::start(move |www| {
        std::fs::write(www.join("big.deb"), &payload).unwrap();
    });
    let r = req(srv.port, "GET", "/big.deb", &["Range: bytes=-100"]);
    assert_eq!(r.status, 206);
    assert_eq!(r.header("Content-Range"), Some("bytes 1048476-1048575/1048576"));
    assert_eq!(r.body, data[1_048_476..]);
}

#[test]
fn unsatisfiable_range_returns_416() {
    let srv = TestServer::start(|www| {
        std::fs::write(www.join("f.bin"), pattern(1000)).unwrap();
    });
    let r = req(srv.port, "GET", "/f.bin", &["Range: bytes=99999999-"]);
    assert_eq!(r.status, 416);
    assert_eq!(r.header("Content-Range"), Some("bytes */1000"));
    assert!(r.body.is_empty());
}

#[test]
fn if_none_match_yields_304() {
    let srv = TestServer::start(|www| {
        std::fs::write(www.join("a.txt"), b"data").unwrap();
    });
    let first = req(srv.port, "GET", "/a.txt", &[]);
    let etag = first.header("ETag").expect("etag").to_string();

    let cond = req(srv.port, "GET", "/a.txt", &[&format!("If-None-Match: {}", etag)]);
    assert_eq!(cond.status, 304);
    assert!(cond.body.is_empty());
    assert_eq!(cond.header("ETag").map(str::to_string), Some(etag));
}

#[test]
fn if_modified_since_future_yields_304() {
    let srv = TestServer::start(|www| {
        std::fs::write(www.join("a.txt"), b"data").unwrap();
    });
    let r = req(
        srv.port,
        "GET",
        "/a.txt",
        &["If-Modified-Since: Sun, 06 Jun 2100 08:49:37 GMT"],
    );
    assert_eq!(r.status, 304);
    assert!(r.body.is_empty());
}

#[test]
fn head_sends_headers_without_body() {
    let srv = TestServer::start(|www| {
        std::fs::write(www.join("big.deb"), pattern(50_000)).unwrap();
    });
    let r = req(srv.port, "HEAD", "/big.deb", &[]);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Content-Length"), Some("50000"));
    assert!(r.body.is_empty());
}

#[test]
fn write_methods_are_rejected() {
    let srv = TestServer::start(|www| {
        std::fs::write(www.join("a.txt"), b"data").unwrap();
    });
    for method in ["POST", "PUT", "DELETE"] {
        let r = req(srv.port, method, "/a.txt", &[]);
        assert_eq!(r.status, 405, "method {} should be rejected", method);
        assert_eq!(r.header("Allow"), Some("GET, HEAD"));
    }
}

#[test]
fn path_traversal_cannot_escape_root() {
    let srv = TestServer::start(|www| {
        std::fs::write(www.join("ok.txt"), b"public").unwrap();
    });
    // Secret one level above the served root.
    std::fs::write(srv.dir.join("secret.txt"), b"TOPSECRET").unwrap();

    for path in ["/../secret.txt", "/%2e%2e/secret.txt"] {
        let r = req(srv.port, "GET", path, &[]);
        assert_ne!(r.status, 200, "{} must not succeed", path);
        assert!(
            !r.body.windows(9).any(|w| w == b"TOPSECRET"),
            "{} leaked secret contents",
            path
        );
    }
}

#[test]
fn empty_path_is_404() {
    let srv = TestServer::start(|_| {});
    let r = req(srv.port, "GET", "/", &[]);
    assert_eq!(r.status, 404);
}

#[test]
fn cache_control_distinguishes_metadata_from_packages() {
    let srv = TestServer::start(|www| {
        let dists = www.join("dists/stable/main/binary-amd64");
        std::fs::create_dir_all(&dists).unwrap();
        std::fs::write(dists.join("Packages.gz"), b"index").unwrap();
        let pool = www.join("pool/main/h/hello");
        std::fs::create_dir_all(&pool).unwrap();
        std::fs::write(pool.join("hello_2.10_amd64.deb"), b"pkg").unwrap();
    });

    let meta = req(srv.port, "GET", "/dists/stable/main/binary-amd64/Packages.gz", &[]);
    assert_eq!(meta.header("Cache-Control"), Some("no-cache"));

    let pkg = req(srv.port, "GET", "/pool/main/h/hello/hello_2.10_amd64.deb", &[]);
    assert_eq!(
        pkg.header("Cache-Control"),
        Some("public, max-age=31536000, immutable")
    );
}
