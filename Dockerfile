# syntax=docker/dockerfile:1

# ---- Build stage ----
FROM rust:1-bookworm AS builder
WORKDIR /app

# Pre-fetch dependencies against a stub so `cargo build` layer-caches on
# Cargo.toml/Cargo.lock changes only, not on every source edit.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# Bust the stub's cached build artifact so the real sources are compiled.
RUN touch src/main.rs && cargo build --release --locked

# ---- Runtime stage ----
FROM debian:bookworm-slim AS runtime

# ca-certificates: HTTPS PMTiles URLs. fonts-dejavu-core: text labels render.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*

# Run as a non-root user.
RUN useradd --system --no-create-home --uid 10001 tiler
USER tiler

COPY --from=builder /app/target/release/tiler /usr/local/bin/tiler

ENV PORT=3000
EXPOSE 3000

ENTRYPOINT ["tiler"]
