# Multi-stage build for the Nuthatch Data Service gateway.
FROM rust:slim-bookworm AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates git && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release --bin nuthatch-gateway

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/nuthatch-gateway /usr/local/bin/nuthatch-gateway
ENV GATEWAY_CONFIG=/app/config.toml
EXPOSE 8090
ENTRYPOINT ["nuthatch-gateway"]
