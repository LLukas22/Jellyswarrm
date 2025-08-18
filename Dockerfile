#################################
# Stage 1: Build Rust Application (Alpine)
#################################
FROM rust:1-alpine AS rust-build

WORKDIR /app

# Install build dependencies (sqlite dev headers, build base, nodejs + npm)
# nodejs & npm from Alpine repo (may lag slightly vs upstream 20.x). If strict Node 20 needed, replace with manual install.
RUN apk add --no-cache \
	build-base \
	curl \
	ca-certificates \
	git \
	pkgconf \
	sqlite-dev \
	openssl-dev \
	nodejs \
	npm

# 2. Cache UI dependencies (copy only package manifests)
COPY ui/package.json ui/package-lock.json* ui/
RUN npm ci --prefix ui || npm install --prefix ui --no-audit --no-fund

# 3. Copy ui sources
COPY ui/ ui/

# 4. Build UI explicitly (not via build.rs) and place into static for rust-embed
RUN npm run build:production --prefix ui \
	&& rm -rf crates/jellyswarrm-proxy/static \
	&& mkdir -p crates/jellyswarrm-proxy/static \
	&& cp -R ui/dist/* crates/jellyswarrm-proxy/static/

# 5. Copy Rust sources and Cargo manifests
COPY Cargo.toml Cargo.lock .cargo ./
COPY crates/jellyswarrm-proxy/Cargo.toml crates/jellyswarrm-proxy/Cargo.toml
COPY crates/jellyswarrm-proxy/askama.toml crates/jellyswarrm-proxy/askama.toml
COPY crates/jellyswarrm-proxy/src crates/jellyswarrm-proxy/src

# Set env var so build.rs skips internal UI build (we already did it)
ENV JELLYSWARRM_SKIP_UI=1

# 5. Build optimized release binary with embedded static assets
RUN cargo build --release --bin jellyswarrm-proxy

#################################
# Stage 2: Runtime Image (Alpine)
#################################
FROM alpine:3.20 AS runtime

WORKDIR /app

ENV HOME=/app
ENV PATH="/app:${PATH}"
ENV RUST_LOG=info

# Install runtime certs + OpenSSL runtime libs + sqlite (for dynamic linking if needed)
RUN apk add --no-cache ca-certificates openssl sqlite-libs \
	&& update-ca-certificates

COPY --from=rust-build /app/target/release/jellyswarrm-proxy /app/jellyswarrm-proxy

EXPOSE 3000

ENTRYPOINT ["/app/jellyswarrm-proxy"]

