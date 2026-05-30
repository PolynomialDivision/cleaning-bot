# syntax=docker/dockerfile:1
# ── Base: chef + build deps ───────────────────────────────────────────────────
FROM rust:1.95-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build

# ── Planner: capture the full dependency graph ────────────────────────────────
FROM chef AS planner
COPY . .
# git cache needed: Cargo.toml patches point at the PolynomialDivision fork
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    cargo chef prepare --recipe-path recipe.json

# ── Builder ───────────────────────────────────────────────────────────────────
FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json

# Cook deps only — this layer is invalidated ONLY when Cargo.lock changes.
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cleaning-bot-target,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cleaning-bot-target,target=/build/target \
    cargo build --release && \
    cp target/release/cleaning-bot /cleaning-bot

# ── Tectonic (LaTeX engine) ───────────────────────────────────────────────────
# tectonic is not in Debian bookworm apt repos; download the static musl binary.
# To upgrade: bump TECTONIC_VERSION.
FROM debian:bookworm-slim AS tectonic
ARG TECTONIC_VERSION=0.15.0
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL \
       "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%40${TECTONIC_VERSION}/tectonic-${TECTONIC_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
       | tar -xz -C /usr/local/bin

# Pre-warm the LaTeX package cache so PDF generation works offline at runtime.
COPY docker/tex-warmup.tex /tmp/tex-warmup.tex
RUN tectonic --outdir /tmp /tmp/tex-warmup.tex \
    && rm /tmp/tex-warmup.tex /tmp/tex-warmup.pdf

# ── Runtime ───────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

# Static musl binary — no extra runtime deps needed.
COPY --from=tectonic /usr/local/bin/tectonic  /usr/local/bin/tectonic
# Pre-warmed LaTeX package cache baked in so first !pdf is instant and offline.
COPY --from=tectonic /root/.cache/Tectonic    /root/.cache/Tectonic

COPY --from=builder  /cleaning-bot            /usr/local/bin/cleaning-bot

VOLUME /app/store
VOLUME /app/config
WORKDIR /app
CMD ["cleaning-bot", "/app/config/config.toml"]
