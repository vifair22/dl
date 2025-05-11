use std::{
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    process::exit,
};

use chrono::Local;
use html_escape::encode_text;
use mime_guess::MimeGuess;
use percent_encoding::percent_decode_str;
use tiny_http::{Header, Response, ResponseBox, Server, StatusCode};

fn list_directory(dir: &Path) -> ResponseBox {
    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            let body = format!("Failed to read directory: {}", e);
            return Response::from_string(body)
                .with_status_code(StatusCode(500))
                .boxed();
        }
    };

    let mut html = String::from("<html><body><h1>Index of /</h1><ul>");
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let esc  = encode_text(&name);
        html.push_str(&format!(r#"<li><a href="/{n}">{n}</a></li>"#, n = esc));
    }
    html.push_str("</ul></body></html>");

    Response::from_string(html)
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .unwrap(),
        )
        .boxed()
}

fn serve_file(base: &Path, rel_path: &str) -> ResponseBox {
    let decoded = percent_decode_str(rel_path).decode_utf8_lossy();
    let path = base.join(&*decoded);

    if !path.starts_with(base) {
        return Response::empty(403).boxed();
    }

    match File::open(&path) {
        Ok(file) => {
            let fname = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");
            let mut resp = Response::from_file(file)
                .with_header(
                    Header::from_bytes(
                        &b"Content-Disposition"[..],
                        format!(r#"attachment; filename="{fname}""#).as_bytes(),
                    )
                    .unwrap(),
                );

            if let Some(m) = MimeGuess::from_path(&path).first() {
                resp = resp.with_header(
                    Header::from_bytes(&b"Content-Type"[..], m.essence_str().as_bytes())
                        .unwrap(),
                );
            }

            resp.boxed()
        }
        Err(_) => Response::empty(404).boxed(),
    }
}

fn main() {
    let dir  = env::var("FILE_SERVER_DIR").unwrap_or_else(|_| "/www".into());
    let port = env::var("PORT").unwrap_or_else(|_| "8000".into());
    let bind = format!("0.0.0.0:{}", port);

    let base = PathBuf::from(&dir);
    if !base.is_dir() {
        eprintln!("ERROR: '{}' is not a directory or does not exist.", dir);
        exit(1);
    }

    let server = Server::http(&bind)
        .unwrap_or_else(|e| panic!("Failed to bind {}: {}", bind, e));
    println!("Serving '{}' on http://{}", dir, bind);

    for request in server.incoming_requests() {
        // Determine client IP, prefer X-Forwarded-For
        let client_ip = request.headers()
            .iter()
            .find(|h| h.field.equiv("X-Forwarded-For"))
            .and_then(|h| std::str::from_utf8(h.value.as_bytes()).ok())
            .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
            .or_else(|| request.remote_addr().map(|a| a.ip().to_string()))
            .unwrap_or_else(|| "-".into());

        // Determine URL and prepare response
        let url = request.url();
        let rel = url.trim_start_matches('/');
        let response = if rel.is_empty() {
            list_directory(&base)
        } else {
            serve_file(&base, rel)
        };

        // Extract status code
        let status_code = response.status_code().0;

        // Current time in YYYY-MM-DD HH:MM
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
    }
}