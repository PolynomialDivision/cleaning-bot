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
# The matrix-sdk fork and all other heavy crates compile here and are cached.
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cleaning-bot-target,target=/build/target \
    cargo chef cook --release --locked --recipe-path recipe.json

COPY . .
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cleaning-bot-target,target=/build/target \
    cargo build --release --locked && \
    cp target/release/cleaning-bot /cleaning-bot

# ── Runtime ───────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libsqlite3-0 \
    chromium \
    fonts-liberation \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /cleaning-bot /usr/local/bin/cleaning-bot

VOLUME /app/store
VOLUME /app/config
WORKDIR /app
CMD ["cleaning-bot", "/app/config/config.toml"]
