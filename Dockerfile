# ── Builder Stage ──
FROM rust:alpine AS builder

# install musl tooling
RUN apk add --no-cache musl-dev

# prepare source
WORKDIR /usr/src/file-server
COPY . .

# add the musl target and build
RUN rustup target add x86_64-unknown-linux-musl \
 && cargo build --release --target x86_64-unknown-linux-musl

# ── Runtime Stage ──
FROM scratch

# copy the static binary
COPY --from=builder /usr/src/file-server/target/x86_64-unknown-linux-musl/release/file-server /file-server

# expose and default-config
EXPOSE 8000
ENV FILE_SERVER_DIR=/www
ENV PORT=8000

ENTRYPOINT ["/file-server"]
