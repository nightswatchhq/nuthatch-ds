# Multi-stage build for the Nuthatch Data Service gateway.
#
# See .dockerignore: gateway.toml and .env are excluded from the build context,
# because the builder stage keeps whatever it is given and the operator key lives
# in one of them.
FROM rust:1.97-slim-bookworm AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates git && rm -rf /var/lib/apt/lists/*
COPY . .
# --locked: the same dependency graph CI tested, including the pinned
# horizon-core commit. A payment gateway does not resolve dependencies freshly
# at image build time.
RUN cargo build --release --locked --bin nuthatch-gateway

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin nuthatch
COPY --from=builder /app/target/release/nuthatch-gateway /usr/local/bin/nuthatch-gateway
USER nuthatch
ENV GATEWAY_CONFIG=/app/config.toml
EXPOSE 8090
ENTRYPOINT ["nuthatch-gateway"]
