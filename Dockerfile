# syntax=docker/dockerfile:1

#################################
# Build the embedded web client
#################################
FROM node:24-alpine AS ui-build
RUN apk add --no-cache git
WORKDIR /app/ui

COPY ui/package.json ui/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci

COPY ui/ ./
COPY .git/modules/ui/ /app/.git/modules/ui/
RUN npm run build:production && test -s dist/index.html
RUN UI_VERSION=$(node -p "require('./package.json').version.replace(/^v/, '')") && \
    UI_COMMIT=$(git rev-parse HEAD) && \
    printf 'UI_VERSION=%s\nUI_COMMIT=%s\n' "$UI_VERSION" "$UI_COMMIT" > dist/ui-version.env

#################################
# Prepare and cache Rust dependencies
#################################
FROM rust:1.99.1-alpine AS chef
WORKDIR /app
RUN apk add --no-cache build-base pkgconf sqlite-dev openssl-dev
RUN cargo install cargo-chef --version 0.1.78 --locked

FROM chef AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS rust-build
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --locked --release --recipe-path recipe.json --bin jellyswarrm-proxy

# Keep target/ in this layer so BuildKit's exported cache includes dependencies.
# cargo-chef handles local crate fingerprints; no manual artifact deletion needed.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/
COPY --from=ui-build /app/ui/dist/ crates/jellyswarrm-proxy/static/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --locked --release --bin jellyswarrm-proxy && \
    cp target/release/jellyswarrm-proxy /app/jellyswarrm-proxy

#################################
# Runtime image
#################################
FROM alpine:3.20 AS runtime
WORKDIR /app
ENV RUST_LOG=info
RUN apk add --no-cache ca-certificates curl sqlite-libs && update-ca-certificates
COPY --from=rust-build /app/jellyswarrm-proxy /app/jellyswarrm-proxy
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl --fail --silent --show-error --max-time 1 "http://127.0.0.1:${JELLYSWARRM_PORT:-3000}/health" > /dev/null \
        && curl --fail --silent --show-error --max-time 3 "http://127.0.0.1:${JELLYSWARRM_PORT:-3000}/ready" > /dev/null \
        || exit 1

ENTRYPOINT ["/app/jellyswarrm-proxy"]
