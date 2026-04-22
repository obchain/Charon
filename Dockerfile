# syntax=docker/dockerfile:1.7

# ─── Builder ──────────────────────────────────────────────────────────
# `rust:1-slim` pairs with a slim Debian runtime below and keeps the
# final image < 150 MB — well under Hetzner CX22's 40 GB disk budget
# even after repeated rebuilds.
FROM rust:1-slim AS builder

# Build-time TLS + pkg-config — alloy transitively links OpenSSL for
# WS over TLS, and reqwest pulls pkg-config during build scripts.
RUN apt-get update \
    && apt-get install --no-install-recommends -y \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Manifests first so the dep layer caches separately from source.
# Dummy `src/main.rs` is cheaper than copying the full workspace when
# only Cargo.* changes — iteration speed on `docker build` during
# compose tuning.
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
# on a 4 GB Hetzner box.
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --no-install-recommends -y \
        ca-certificates \
        libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /app --shell /usr/sbin/nologin charon

WORKDIR /app
COPY --from=builder /charon /usr/local/bin/charon
COPY config ./config

USER charon

EXPOSE 9091

ENTRYPOINT ["charon"]
CMD ["--config", "config/default.toml", "listen"]
