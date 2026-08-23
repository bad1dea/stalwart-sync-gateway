FROM rust:bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin gateway \
    && mkdir -p /data \
    && chown 10001:10001 /data
COPY --from=builder /app/target/release/stalwart-sync-gateway /usr/local/bin/stalwart-sync-gateway
USER 10001:10001
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/stalwart-sync-gateway"]
