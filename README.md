# file-server

A small static file server (Rust + tiny_http) used as the backend for
`dl.airies.net`, including its role as a Debian apt and Gentoo binary-package
(binhost) repository. It serves a read-only directory tree over HTTP with the
behaviour package managers rely on: byte ranges for resumable downloads,
conditional requests (`ETag` / `Last-Modified` -> `304`), and correct
`Content-Length` on large files.

It only serves the files under the configured root. Generating the repository
metadata (apt's `Release` / `Packages`, Gentoo's `Packages` index) and signing
it is done by separate tooling; this server just serves whatever static tree it
is pointed at.

## Configuration

All configuration is via environment variables:

| Variable          | Default | Description                                                                 |
|-------------------|---------|-----------------------------------------------------------------------------|
| `FILE_SERVER_DIR` | `/www`  | Directory to serve. The process exits if it does not exist.                 |
| `PORT`            | `8000`  | TCP port to bind on `0.0.0.0`. `0` lets the OS pick (printed at startup).    |
| `SERVER_THREADS`  | `64`    | Max concurrent transfers. Clamped to `1..=1024`. See note below.            |

`SERVER_THREADS` bounds how many responses are streamed at once, not the total
thread or connection count (tiny_http maintains its own elastic per-connection
pool). Serving is IO-bound, so this is limited in practice by network and disk
throughput rather than CPU count; the default of 64 is ample for a LAN of apt /
portage clients.

## Behaviour

- Only `GET` and `HEAD` are served; other methods return `405`.
- Byte ranges (`Range: bytes=start-`, `start-end`, and suffix `-N`) return `206`
  with `Content-Range`; unsatisfiable ranges return `416`.
- `If-None-Match` and `If-Modified-Since` return `304` when the client copy is
  current.
- `Cache-Control` is `no-cache` for repo indexes (apt files under `dists/`, the
  Gentoo `Packages` index) and long-lived `immutable` for package payloads.
- Path traversal outside the served root is rejected.

## Build and run (Docker)

Build:

    docker build -t airies-dl .

Run:

    docker run -d --name airies-dl -p 8000:8000 -v ~/local/dl/www:/www:ro airies-dl

Override the concurrency cap:

    docker run -d --name airies-dl -p 8000:8000 -e SERVER_THREADS=128 \
      -v ~/local/dl/www:/www:ro airies-dl

## Development

    cargo build --release
    cargo test
    cargo clippy --all-targets
