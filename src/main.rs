use std::{
    env,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::exit,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use chrono::{DateTime, Local, NaiveDateTime, Utc};
use mime_guess::MimeGuess;
use percent_encoding::percent_decode_str;
use threadpool::ThreadPool;
use tiny_http::{Header, Method, Request, Response, ResponseBox, Server, StatusCode};

// Build a header from static-ish byte slices. The inputs we pass are always
// valid header field/value bytes, so a failure here is a programming error.
fn hdr(field: &[u8], value: &[u8]) -> Header {
    Header::from_bytes(field, value).expect("valid header")
}

// First value of a request header, decoded as UTF-8. None if absent or non-UTF-8.
fn header_value(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(h.value.as_bytes()).ok())
        .map(|s| s.to_string())
}

// Format a SystemTime as an HTTP IMF-fixdate (RFC 7231),
// e.g. "Sun, 06 Jun 2026 08:49:37 GMT". chrono renders English weekday/month
// abbreviations, which is exactly what the HTTP date grammar requires.
fn http_date(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

// Parse an HTTP date into whole seconds since the Unix epoch. Only the
// IMF-fixdate form is accepted; apt and portage both send that. Returns None on
// anything else, which the caller treats as "no usable If-Modified-Since".
fn parse_http_date(s: &str) -> Option<u64> {
    let naive = NaiveDateTime::parse_from_str(s.trim(), "%a, %d %b %Y %H:%M:%S GMT").ok()?;
    u64::try_from(naive.and_utc().timestamp()).ok()
}

fn system_time_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// Cache policy by repo path. Package payloads are content-versioned and never
// change once published, so they cache hard. Repo indexes (apt's files under
// dists/, the Gentoo `Packages` index) change in place, so they must revalidate
// every time; the 304 path keeps that revalidation cheap.
fn cache_control(rel: &str) -> &'static str {
    let p = rel.trim_start_matches('/');
    let basename = p.rsplit('/').next().unwrap_or(p);
    let is_metadata = p.contains("dists/")
        || matches!(basename, "Packages" | "Packages.gz" | "Packages.xz");
    if is_metadata {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    }
}

fn range_not_satisfiable(total: u64) -> ResponseBox {
    Response::empty(416)
        .with_header(hdr(b"Content-Range", format!("bytes */{}", total).as_bytes()))
        .boxed()
}

// Parse a single RFC 7233 byte-range-spec against a known entity size. Returns an
// inclusive (start, end) that is guaranteed satisfiable, or None when the range
// is malformed or cannot be satisfied (the caller answers 416 in that case).
// Supports `bytes=start-`, `bytes=start-end`, and suffix `bytes=-N`.
fn parse_single_range(spec: &str, total: u64) -> Option<(u64, u64)> {
    let (start_s, end_s) = spec.trim().split_once('-')?;

    if start_s.is_empty() {
        // suffix range: the final N bytes
        let n: u64 = end_s.parse().ok()?;
        if n == 0 || total == 0 {
            return None;
        }
        let n = n.min(total);
        return Some((total - n, total - 1));
    }

    let start: u64 = start_s.parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if end_s.is_empty() {
        total - 1
    } else {
        end_s.parse::<u64>().ok()?.min(total - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

fn serve_file(base: &Path, rel_path: &str, req: &Request) -> ResponseBox {
    let path_str = rel_path.split('?').next().unwrap_or("");
    let decoded = percent_decode_str(path_str).decode_utf8_lossy();
    let path = base.join(&*decoded);

    let resolved = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return Response::empty(404).boxed(),
    };
    if !resolved.starts_with(base) || resolved.is_dir() {
        return Response::empty(403).boxed();
    }

    let meta = match fs::metadata(&resolved) {
        Ok(m) => m,
        Err(_) => return Response::empty(404).boxed(),
    };
    let total_size = meta.len();

    // Validators for conditional requests / caching.
    let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
    let mtime_secs = system_time_secs(mtime);
    let last_modified = http_date(mtime);
    let etag = format!("\"{:x}-{:x}\"", mtime_secs, total_size);
    let cache = cache_control(&decoded);

    // Conditional request handling (RFC 7232). If-None-Match takes precedence
    // over If-Modified-Since. A match means the client's cached copy is current,
    // so we answer 304 with the validators and no body. This is what makes
    // `apt update` and a Gentoo sync cheap instead of re-pulling metadata each time.
    let not_modified = if let Some(inm) = header_value(req, "If-None-Match") {
        inm.trim() == "*"
            || inm
                .split(',')
                .any(|t| t.trim().trim_start_matches("W/") == etag)
    } else if let Some(ims) = header_value(req, "If-Modified-Since") {
        parse_http_date(&ims).is_some_and(|ims_secs| mtime_secs <= ims_secs)
    } else {
        false
    };

    if not_modified {
        return Response::empty(304)
            .with_header(hdr(b"ETag", etag.as_bytes()))
            .with_header(hdr(b"Last-Modified", last_modified.as_bytes()))
            .with_header(hdr(b"Cache-Control", cache.as_bytes()))
            .with_header(hdr(b"Accept-Ranges", b"bytes"))
            .boxed();
    }

    let mut file = match File::open(&resolved) {
        Ok(f) => f,
        Err(_) => return Response::empty(404).boxed(),
    };

    let mut headers = vec![
        hdr(b"Accept-Ranges", b"bytes"),
        hdr(b"ETag", etag.as_bytes()),
        hdr(b"Last-Modified", last_modified.as_bytes()),
        hdr(b"Cache-Control", cache.as_bytes()),
        hdr(
            b"Content-Disposition",
            format!(
                r#"attachment; filename="{}""#,
                resolved.file_name().and_then(|n| n.to_str()).unwrap_or("file")
            )
            .as_bytes(),
        ),
    ];

    if let Some(m) = MimeGuess::from_path(&resolved).first() {
        headers.push(hdr(b"Content-Type", m.essence_str().as_bytes()));
    }

    // Range handling (RFC 7233): a single byte range, normal or suffix form.
    // Anything we cannot satisfy gets a 416 with `Content-Range: bytes */total`
    // so a resuming client can recover instead of silently receiving a full 200.
    if let Some(range_val) = header_value(req, "Range") {
        if let Some(spec) = range_val.strip_prefix("bytes=") {
            // Multiple ranges are not supported; reject rather than mis-serve.
            if spec.contains(',') {
                return range_not_satisfiable(total_size);
            }
            let (start, end) = match parse_single_range(spec, total_size) {
                Some(pair) => pair,
                None => return range_not_satisfiable(total_size),
            };
            let chunk_size = end - start + 1;
            let _ = file.seek(SeekFrom::Start(start));
            let reader = file.take(chunk_size);

            headers.push(hdr(
                b"Content-Range",
                format!("bytes {}-{}/{}", start, end, total_size).as_bytes(),
            ));

            // with_chunked_threshold(MAX) keeps tiny_http on identity encoding so
            // we always emit Content-Length. Its 32 KB default would otherwise
            // chunk every package and drop Content-Length, breaking resume.
            return Response::new(StatusCode(206), headers, reader, Some(chunk_size as usize), None)
                .with_chunked_threshold(usize::MAX)
                .boxed();
        }
    }

    Response::new(StatusCode(200), headers, file, Some(total_size as usize), None)
        .with_chunked_threshold(usize::MAX)
        .boxed()
}

// Maximum concurrent transfers. This bounds how many responses we stream at
// once, not the total thread or connection count (tiny_http keeps its own
// elastic per-connection pool). File serving is IO-bound, so concurrency is
// limited by the NIC and disk rather than CPU count; a flat default is the right
// shape. The pool eagerly spawns this many threads at startup, so the override is
// clamped to keep a typo from exhausting resources before the first byte serves.
const DEFAULT_THREADS: usize = 64;
const MAX_THREADS: usize = 1024;

fn num_threads() -> usize {
    env::var("SERVER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .map(|n| n.min(MAX_THREADS))
        .unwrap_or(DEFAULT_THREADS)
}

fn main() {
    let dir = env::var("FILE_SERVER_DIR").unwrap_or_else(|_| "/www".into());
    let base_raw = PathBuf::from(&dir);
    if !base_raw.is_dir() {
        eprintln!("ERROR: '{}' is not a directory or does not exist.", dir);
        exit(1);
    }
    let base_canon = match base_raw.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERROR: could not resolve '{}': {}", dir, e);
            exit(1);
        }
    };
    let base = Arc::new(base_canon);

    let port = env::var("PORT").unwrap_or_else(|_| "8000".into());
    let bind = format!("0.0.0.0:{}", port);
    let server = Server::http(&bind)
        .unwrap_or_else(|e| panic!("Failed to bind {}: {}", bind, e));
    let addr = server
        .server_addr()
        .to_ip()
        .map(|a| a.to_string())
        .unwrap_or(bind);
    println!("Serving '{}' on http://{}", dir, addr);

    let pool = ThreadPool::new(num_threads());

    for request in server.incoming_requests() {
        let base = Arc::clone(&base);
        pool.execute(move || {
            let client_ip = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("X-Forwarded-For"))
                .and_then(|h| std::str::from_utf8(h.value.as_bytes()).ok())
                .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
                .or_else(|| request.remote_addr().map(|a| a.ip().to_string()))
                .unwrap_or_else(|| "-".into());

            let url = request.url().to_string();
            let rel = url.trim_start_matches('/');

            // Read-only binhost: serve GET/HEAD (tiny_http strips the body for
            // HEAD itself, keeping Content-Length), reject everything else.
            let response = match request.method() {
                Method::Get | Method::Head => {
                    if rel.is_empty() {
                        Response::empty(404).boxed()
                    } else {
                        serve_file(&base, rel, &request)
                    }
                }
                _ => Response::empty(405)
                    .with_header(hdr(b"Allow", b"GET, HEAD"))
                    .boxed(),
            };
            let status_code = response.status_code().0;

            // Logging must never take down a request: a broken stdout pipe (log
            // rotation, a dropped supervisor, etc.) would otherwise panic the
            // worker, so we swallow any write error here.
            let now = Local::now().format("%Y-%m-%d %H:%M");
            let _ = writeln!(
                io::stdout(),
                "[{}] {} \"{}\" from {} {{{}}}",
                now,
                request.method(),
                url,
                client_ip,
                status_code
            );

            let _ = request.respond(response);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_normal_open_ended() {
        assert_eq!(parse_single_range("100-", 1000), Some((100, 999)));
    }

    #[test]
    fn range_normal_closed() {
        assert_eq!(parse_single_range("100-199", 1000), Some((100, 199)));
    }

    #[test]
    fn range_end_clamped_to_size() {
        // An end past EOF is clamped to the last byte, not rejected.
        assert_eq!(parse_single_range("100-99999", 1000), Some((100, 999)));
    }

    #[test]
    fn range_suffix() {
        assert_eq!(parse_single_range("-100", 1000), Some((900, 999)));
    }

    #[test]
    fn range_suffix_larger_than_file() {
        // Asking for more trailing bytes than exist yields the whole file.
        assert_eq!(parse_single_range("-5000", 1000), Some((0, 999)));
    }

    #[test]
    fn range_start_at_or_past_eof_is_unsatisfiable() {
        assert_eq!(parse_single_range("1000-", 1000), None);
        assert_eq!(parse_single_range("1001-1100", 1000), None);
    }

    #[test]
    fn range_zero_suffix_and_empty_file() {
        assert_eq!(parse_single_range("-0", 1000), None);
        assert_eq!(parse_single_range("-100", 0), None);
        assert_eq!(parse_single_range("0-", 0), None);
    }

    #[test]
    fn range_malformed() {
        assert_eq!(parse_single_range("abc", 1000), None);
        assert_eq!(parse_single_range("-", 1000), None);
        assert_eq!(parse_single_range("10-5", 1000), None); // end < start
        assert_eq!(parse_single_range("x-y", 1000), None);
    }

    #[test]
    fn http_date_format_is_imf_fixdate() {
        // 1_780_084_005 == 2026-05-29 19:46:45 UTC.
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_780_084_005);
        assert_eq!(http_date(t), "Fri, 29 May 2026 19:46:45 GMT");
    }

    #[test]
    fn http_date_round_trips_through_parse() {
        let secs = 1_780_084_005u64;
        let t = UNIX_EPOCH + std::time::Duration::from_secs(secs);
        assert_eq!(parse_http_date(&http_date(t)), Some(secs));
    }

    #[test]
    fn parse_http_date_rejects_garbage() {
        assert_eq!(parse_http_date("not a date"), None);
        assert_eq!(parse_http_date("2026-06-10T20:03:25Z"), None); // ISO 8601, not HTTP
    }

    #[test]
    fn cache_control_metadata_revalidates() {
        assert_eq!(cache_control("dists/stable/InRelease"), "no-cache");
        assert_eq!(cache_control("dists/stable/main/binary-amd64/Packages.gz"), "no-cache");
        assert_eq!(cache_control("debian/dists/trixie/Release"), "no-cache");
        assert_eq!(cache_control("Packages"), "no-cache"); // Gentoo index at root
        assert_eq!(cache_control("gentoo/amd64/Packages.gz"), "no-cache");
    }

    #[test]
    fn cache_control_packages_are_immutable() {
        let immutable = "public, max-age=31536000, immutable";
        assert_eq!(cache_control("pool/main/h/hello/hello_2.10_amd64.deb"), immutable);
        assert_eq!(cache_control("app-misc/foo-1.2.3-1.gpkg.tar"), immutable);
        assert_eq!(cache_control("sys-apps/bar-4.0.tbz2"), immutable);
    }
}
