use std::{
    env,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::exit,
    sync::Arc,
};
use chrono::Local;
use mime_guess::MimeGuess;
use percent_encoding::percent_decode_str;
use threadpool::ThreadPool;
use tiny_http::{Header, Request, Response, ResponseBox, Server, StatusCode};

fn serve_file(base: &Path, rel_path: &str, req: &Request) -> ResponseBox {
    let path_str = rel_path.split('?').next().unwrap_or("");
    let decoded = percent_decode_str(path_str).decode_utf8_lossy();
    let path = base.join(&*decoded);

    let resolved = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return Response::empty(403).boxed(),
    };
    if !resolved.starts_with(base) || resolved.is_dir() {
        return Response::empty(403).boxed();
    }

    let mut file = match File::open(&resolved) {
        Ok(f) => f,
        Err(_) => return Response::empty(404).boxed(),
    };

    let total_size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Response::empty(500).boxed(),
    };

    let mut headers = vec![
        Header::from_bytes(b"Accept-Ranges", b"bytes").unwrap(),
        Header::from_bytes(
            b"Content-Disposition",
            format!(
                r#"attachment; filename="{}""#,
                resolved.file_name().and_then(|n| n.to_str()).unwrap_or("file")
            )
            .as_bytes(),
        )
        .unwrap(),
    ];

    if let Some(m) = MimeGuess::from_path(&resolved).first() {
        headers.push(Header::from_bytes(b"Content-Type", m.essence_str().as_bytes()).unwrap());
    }

    if let Some(range_hdr) = req.headers().iter().find(|h| h.field.equiv("Range")) {
        if let Ok(val) = std::str::from_utf8(range_hdr.value.as_bytes()) {
            if let Some(range) = val.strip_prefix("bytes=") {
                let parts: Vec<_> = range.splitn(2, '-').collect();
                if let Ok(start) = parts[0].parse::<u64>() {
                    let end = parts
                        .get(1)
                        .and_then(|e| e.parse::<u64>().ok())
                        .filter(|&e| e >= start && e < total_size)
                        .unwrap_or(total_size - 1);
                    let chunk_size = end - start + 1;
                    let _ = file.seek(SeekFrom::Start(start));
                    let reader = file.take(chunk_size);

                    headers.push(
                        Header::from_bytes(
                            b"Content-Range",
                            format!("bytes {}-{}/{}", start, end, total_size).as_bytes(),
                        )
                        .unwrap(),
                    );

                    return Response::new(
                        StatusCode(206),
                        headers,
                        reader,
                        Some(chunk_size.try_into().unwrap()),
                        None,
                    )
                    .boxed();
                }
            }
        }
    }

    Response::new(
        StatusCode(200),
        headers,
        file,
        Some(total_size.try_into().unwrap()),
        None,
    )
    .boxed()
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
    println!("Serving '{}' on http://{}", dir, bind);

    let pool = ThreadPool::new(num_cpus::get());

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

            let response = if rel.is_empty() {
                Response::empty(404).boxed()
            } else {
                serve_file(&base, rel, &request)
            };
            let status_code = response.status_code().0;

            let now = Local::now().format("%Y-%m-%d %H:%M");
            println!(
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