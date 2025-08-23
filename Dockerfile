#################################
# Stage 1: Build UI (Node.js optimized)
#################################
FROM node:20-alpine AS ui-build

WORKDIR /app/ui

# Copy package files for dependency caching
COPY ui/package.json ui/package-lock.json* ./

# Install all dependencies (including dev deps needed for build)
RUN --mount=type=cache,target=/root/.npm \
    npm ci --ignore-scripts

# Copy UI source code
COPY ui/ ./

# Build production UI bundle
RUN npm run build:production

#################################
# Stage 2: Rust Dependencies Cache
#################################
FROM rust:1-alpine AS rust-deps

WORKDIR /app

# Install build dependencies
RUN apk add --no-cache \
	build-base \
	pkgconf \
	sqlite-dev \
	openssl-dev

# Copy Cargo manifests for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/jellyswarrm-proxy/Cargo.toml crates/jellyswarrm-proxy/Cargo.toml

# Create dummy source files to build dependencies
RUN mkdir -p crates/jellyswarrm-proxy/src \
	&& echo "fn main() {}" > crates/jellyswarrm-proxy/src/main.rs \
	&& echo "" > crates/jellyswarrm-proxy/src/lib.rs

# Build dependencies only (will be cached)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/tmp/target,sharing=locked \
    CARGO_TARGET_DIR=/tmp/target cargo build --release --bin jellyswarrm-proxy \
	&& cp /tmp/target/release/jellyswarrm-proxy /app/jellyswarrm-proxy-deps \
	&& rm -rf crates/jellyswarrm-proxy/src

#################################
# Stage 3: Build Rust Application
#################################
FROM rust-deps AS rust-build

# Set env var so build.rs skips internal UI build (we already did it)
ENV JELLYSWARRM_SKIP_UI=1

# Copy UI build artifacts from stage 1
COPY --from=ui-build /app/ui/dist crates/jellyswarrm-proxy/static/

# Copy Rust source code and configuration
COPY crates/jellyswarrm-proxy/askama.toml crates/jellyswarrm-proxy/askama.toml
COPY crates/jellyswarrm-proxy/src crates/jellyswarrm-proxy/src

# Build only the application code (dependencies already cached)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/tmp/target,sharing=locked \
    CARGO_TARGET_DIR=/tmp/target cargo build --release --bin jellyswarrm-proxy \
    && cp /tmp/target/release/jellyswarrm-proxy /app/jellyswarrm-proxy

#################################
# Stage 4: Runtime Image (Alpine)
#################################
FROM alpine:3.20 AS runtime

WORKDIR /app

ENV RUST_LOG=info

# Install minimal runtime dependencies
RUN apk add --no-cache \
	ca-certificates \
	sqlite-libs \
	&& update-ca-certificates

# Copy the compiled binary
COPY --from=rust-build /app/jellyswarrm-proxy /app/jellyswarrm-proxy

EXPOSE 3000

ENTRYPOINT ["/app/jellyswarrm-proxy"]

