FROM rust:1.94 AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/pastebin /usr/local/bin/pastebin

ENV PASTEBIN_BIND=0.0.0.0:8080
EXPOSE 8080

VOLUME /data
ENV PASTEBIN_DB_PATH=/data/pastebin.db

ENTRYPOINT ["pastebin"]
