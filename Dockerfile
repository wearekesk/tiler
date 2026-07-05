# syntax=docker/dockerfile:1

# ---- Build stage ----
FROM rust:1-alpine AS builder
WORKDIR /app

# musl build deps: build-base gives gcc/musl-dev/make; cmake + perl are needed to
# compile aws-lc-sys (rustls' crypto provider); linux-headers is used by its C
# build. Rust on Alpine targets musl and links statically, so the runtime image
# needs none of these.
RUN apk add --no-cache build-base cmake perl linux-headers

# Pre-fetch and compile dependencies against a stub so this layer caches on
# Cargo.toml/Cargo.lock changes only, not on every source edit.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY assets ./assets
# Bust the stub's cached build artifact so the real sources are compiled.
RUN touch src/main.rs && cargo build --release --locked

# ---- Runtime stage ----
FROM alpine:3 AS runtime

# ca-certificates: rustls (via rustls-platform-verifier) reads the system trust
# store to validate HTTPS PMTiles URLs. Label fonts (Roboto) are embedded in the
# binary, so no font package is installed.
RUN apk add --no-cache ca-certificates

# Run as a non-root system user (no password, no home directory).
RUN adduser -D -H -u 10001 tiler
USER tiler

COPY --from=builder /app/target/release/tiler /usr/local/bin/tiler

ENV PORT=3000
EXPOSE 3000

ENTRYPOINT ["tiler"]
