# ── Builder Stage ──
FROM rust:alpine AS builder

# Static linking against musl
RUN apk add --no-cache musl-dev
WORKDIR /usr/src/file-server
COPY . .
ENV RUSTFLAGS="-C link-arg=-s"

RUN rustup target add x86_64-unknown-linux-musl \
 && cargo build --release --target x86_64-unknown-linux-musl

# ── Runtime Stage ──
FROM alpine
STOPSIGNAL SIGTERM

# 1. Bring in Supervisor
RUN apk add --no-cache supervisor

# 2. Add an unprivileged user (optional but traditional)
RUN adduser -D -H -s /sbin/nologin appuser

# 3. Copy the binary produced in the first stage
COPY --from=builder /usr/src/file-server/target/x86_64-unknown-linux-musl/release/file-server \
      /usr/local/bin/file-server
RUN chown appuser:appuser /usr/local/bin/file-server

# 4. Default directories & permissions
ENV FILE_SERVER_DIR=/www
ENV PORT=8000
# Max concurrent transfers. Override at `docker run -e SERVER_THREADS=N`.
ENV SERVER_THREADS=64
RUN mkdir -p "$FILE_SERVER_DIR" /var/log/supervisor \
 && chown -R appuser:appuser "$FILE_SERVER_DIR"

# 5. Supervisor configuration files
COPY supervisord.conf /etc/supervisord.conf

EXPOSE 8000

# 6. Start Supervisor as PID 1
CMD ["/usr/bin/supervisord", "-c", "/etc/supervisord.conf"]
