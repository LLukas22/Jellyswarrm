#################################
# Stage 1: Build UI (Node.js optimized)
#################################
FROM node:20-alpine AS ui-build

# Install git for version detection
RUN apk add --no-cache git

WORKDIR /app/ui

# Copy package files for dependency caching
COPY ui/package.json ui/package-lock.json* ./

# Install all dependencies (including dev deps needed for build)
RUN --mount=type=cache,target=/root/.npm \
    npm install --engine-strict=false --ignore-scripts

# Copy UI source code and git metadata
COPY ui/ ./
COPY .git/modules/ui/ /app/.git/modules/ui/

# Get and print UI version info
RUN UI_VERSION=$(git describe --tags) && \
    UI_COMMIT=$(git rev-parse HEAD) && \
    echo "UI_VERSION=$UI_VERSION" && \
    echo "UI_COMMIT=$UI_COMMIT"

# Build production UI bundle
RUN npm run build:production

# Write ui-version.env file
RUN UI_VERSION=$(git describe --tags) && \
    UI_COMMIT=$(git rev-parse HEAD) && \
    printf "UI_VERSION=%s\nUI_COMMIT=%s\n" "$UI_VERSION" "$UI_COMMIT" > dist/ui-version.env && \
    echo "Generated dist/ui-version.env"



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
COPY .cargo .cargo
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/jellyswarrm-proxy/Cargo.toml crates/jellyswarrm-proxy/Cargo.toml
COPY crates/jellyswarrm-macros/Cargo.toml crates/jellyswarrm-macros/Cargo.toml

# Create dummy source files to build dependencies
RUN mkdir -p crates/jellyswarrm-proxy/src crates/jellyswarrm-macros/src \
	&& echo "fn main() {}" > crates/jellyswarrm-proxy/src/main.rs \
	&& echo "" > crates/jellyswarrm-proxy/src/lib.rs \
	&& echo "use proc_macro::TokenStream; #[proc_macro_attribute] pub fn multi_case_struct(_args: TokenStream, input: TokenStream) -> TokenStream { input }" > crates/jellyswarrm-macros/src/lib.rs

# Build dependencies only (will be cached)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/tmp/target,sharing=locked \
    CARGO_TARGET_DIR=/tmp/target cargo build --release --bin jellyswarrm-proxy \
	&& cp /tmp/target/release/jellyswarrm-proxy /app/jellyswarrm-proxy-deps \
	&& rm -rf crates/jellyswarrm-proxy/src crates/jellyswarrm-macros/src

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
COPY crates/jellyswarrm-proxy/migrations crates/jellyswarrm-proxy/migrations
COPY crates/jellyswarrm-macros/src crates/jellyswarrm-macros/src

# Build only the application code (dependencies already cached)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/tmp/target,sharing=locked \
    rm -rf /tmp/target/release/deps/libjellyswarrm_macros* /tmp/target/release/deps/jellyswarrm_macros* \
    && touch crates/jellyswarrm-macros/src/lib.rs \
    && CARGO_TARGET_DIR=/tmp/target cargo build --release --bin jellyswarrm-proxy \
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

