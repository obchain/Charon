# syntax=docker/dockerfile:1.7

# ─── Builder ──────────────────────────────────────────────────────────
# `rust:1-slim` pairs with a slim Debian runtime below and keeps the
# final image < 150 MB — well under Hetzner CX22's 40 GB disk budget
# even after repeated rebuilds.
#
# Pinned by digest so a Docker Hub re-tag cannot silently swap the
# toolchain, libc, or OpenSSL under a rebuild. Bump in a dedicated
# commit when refreshing the base — never as a drive-by.
FROM rust:1-slim@sha256:c03ea1587a8e4474ae1a3f4a377cbb35ad53d2eb5c27f0bdf1ca8986025e322f AS builder

# Build-time TLS + pkg-config — alloy transitively links OpenSSL for
# WS over TLS, and reqwest pulls pkg-config during build scripts.
RUN apt-get update \
    && apt-get install --no-install-recommends -y \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the full workspace. The BuildKit cache mount on `/build/target`
# below is what preserves incremental compilation across rebuilds —
# `COPY crates` invalidates this layer on any source change, but the
# RUN layer reuses the cached `target/` so cargo only recompiles
# crates whose source actually changed. `Cargo.lock` churn still
# forces a full dep recompile (5-15 min on CX22 2 vCPU).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build the release binary. `charon` is the single bin — other crates
# are libraries and compile as dependencies of it.
RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --locked --release --bin charon \
    && cp target/release/charon /charon

# ─── Runtime ──────────────────────────────────────────────────────────
# `debian:bookworm-slim` because we need CA certificates and libssl3
# for outbound TLS (WS RPC, Chainlink HTTP). Distroless is smaller but
# drops the shell, which makes `docker compose exec` diagnostics harder
# on a 4 GB Hetzner box. Digest-pinned for the same reason as the
# builder — predictable libssl3 ABI across rebuilds.
FROM debian:bookworm-slim@sha256:f9c6a2fd2ddbc23e336b6257a5245e31f996953ef06cd13a59fa0a1df2d5c252 AS runtime

RUN apt-get update \
    && apt-get install --no-install-recommends -y \
        ca-certificates \
        curl \
        libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /app --shell /usr/sbin/nologin charon

WORKDIR /app
COPY --from=builder /charon /usr/local/bin/charon

# `config/` is deliberately not copied into the image: compose always
# bind-mounts `../../config:/app/config:ro` at runtime, and a
# `docker run` without a mount would otherwise launch silently against
# stale TOML (contract addresses, RPC endpoints) or leak secrets
# baked into a layer. Running the image without a config mount fails
# at startup, which is the intended behaviour — see #287.

USER charon

EXPOSE 9091

# Probe the Prometheus exporter — the final step in the bot's startup
# sequence, so a 200 on /metrics implies RPC connect + chain-id check
# + listener bind all succeeded. `start-period` covers the WS
# handshake + first block drain on a cold start.
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -sf http://localhost:9091/metrics > /dev/null || exit 1

ENTRYPOINT ["charon"]
CMD ["--config", "config/default.toml", "listen"]
