#!/usr/bin/env bash
set -euo pipefail

# Build a fully static x86_64-unknown-linux-musl binary inside a container.
# Output: target/x86_64-unknown-linux-musl/release/zeroclaw
#
# Optional env:
#   ZEROCLAW_CARGO_FEATURES  extra cargo features (comma-separated)
#   CONTAINER_CMD            override container runtime (default: auto-detect)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

TARGET="x86_64-unknown-linux-musl"
OUT_DIR="$REPO_DIR/target/$TARGET/release"

info() { echo "==> $*"; }

# Auto-detect container runtime
if [ -n "${CONTAINER_CMD:-}" ]; then
  CTR="$CONTAINER_CMD"
elif command -v docker >/dev/null 2>&1; then
  CTR=docker
elif command -v podman >/dev/null 2>&1; then
  CTR=podman
else
  echo "error: docker or podman required" >&2
  exit 1
fi

info "Building static musl binary via $CTR"

mkdir -p "$OUT_DIR"

$CTR build \
  --build-arg "ZEROCLAW_CARGO_FEATURES=${ZEROCLAW_CARGO_FEATURES:-}" \
  --target static-out \
  --output "type=local,dest=$OUT_DIR" \
  -f - "$REPO_DIR" <<'DOCKERFILE'
# syntax=docker/dockerfile:1.7

FROM docker.io/library/rust:1.93-alpine AS builder

RUN apk add --no-cache musl-dev pkgconf git

WORKDIR /app
ARG ZEROCLAW_CARGO_FEATURES=""

# 1. Cache dependencies via dummy sources
COPY Cargo.toml Cargo.lock build.rs ./
COPY crates/robot-kit/Cargo.toml crates/robot-kit/Cargo.toml
COPY crates/zeroclaw-types/Cargo.toml crates/zeroclaw-types/Cargo.toml
COPY crates/zeroclaw-core/Cargo.toml crates/zeroclaw-core/Cargo.toml
RUN mkdir -p src benches \
      crates/robot-kit/src crates/zeroclaw-types/src crates/zeroclaw-core/src \
    && echo "fn main() {}" > src/main.rs \
    && echo "fn main() {}" > benches/agent_benchmarks.rs \
    && echo "pub fn placeholder() {}" > crates/robot-kit/src/lib.rs \
    && echo "pub fn placeholder() {}" > crates/zeroclaw-types/src/lib.rs \
    && echo "pub fn placeholder() {}" > crates/zeroclaw-core/src/lib.rs

RUN --mount=type=cache,id=musl-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=musl-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=musl-target,target=/app/target,sharing=locked \
    if [ -n "$ZEROCLAW_CARGO_FEATURES" ]; then \
      cargo build --release --target x86_64-unknown-linux-musl --features "$ZEROCLAW_CARGO_FEATURES"; \
    else \
      cargo build --release --target x86_64-unknown-linux-musl --locked; \
    fi

RUN rm -rf src benches crates/robot-kit/src crates/zeroclaw-types/src crates/zeroclaw-core/src

# 2. Build real sources
COPY src/ src/
COPY benches/ benches/
COPY crates/ crates/
COPY data/ data/
COPY firmware/ firmware/
COPY templates/ templates/
COPY web/ web/
RUN mkdir -p web/dist && \
    if [ ! -f web/dist/index.html ]; then \
      printf '%s\n' \
        '<!doctype html><html><head><title>ZeroClaw</title></head>' \
        '<body><p>Frontend not bundled.</p></body></html>' \
        > web/dist/index.html; \
    fi

RUN --mount=type=cache,id=musl-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=musl-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=musl-target,target=/app/target,sharing=locked \
    if [ -n "$ZEROCLAW_CARGO_FEATURES" ]; then \
      cargo build --release --target x86_64-unknown-linux-musl --features "$ZEROCLAW_CARGO_FEATURES"; \
    else \
      cargo build --release --target x86_64-unknown-linux-musl --locked; \
    fi && \
    cp target/x86_64-unknown-linux-musl/release/zeroclaw /app/zeroclaw && \
    strip /app/zeroclaw

# 3. Export binary only
FROM scratch AS static-out
COPY --from=builder /app/zeroclaw /zeroclaw
DOCKERFILE

# Docker --output puts it at $OUT_DIR/zeroclaw
info "Binary: $OUT_DIR/zeroclaw"
file "$OUT_DIR/zeroclaw"
