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
# on a 4 GB Hetzner box. Digest-pinned for the same reason as the
# builder — predictable libssl3 ABI across rebuilds.
FROM debian:bookworm-slim@sha256:f9c6a2fd2ddbc23e336b6257a5245e31f996953ef06cd13a59fa0a1df2d5c252 AS runtime

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
