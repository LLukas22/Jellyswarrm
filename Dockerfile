#################################
# Stage 1: Build via Nix flake
#################################
FROM nixos/nix:latest AS builder

# Seed the Nix store cache from the base image the first time it is used.
# The same cache IDs are mounted over /nix/store and /nix/var/nix in the build
# step below; without this seed, the empty cache mount would shadow the base
# image layer, making /bin/sh (a symlink into /nix/store) unreachable.
# The .nix-seeded sentinel prevents redundant copies on subsequent builds.
RUN --mount=type=cache,id=jellyswarrm-nix-store,target=/nix/store-init,sharing=locked \
  --mount=type=cache,id=jellyswarrm-nix-var,target=/nix/var-init,sharing=locked \
  if [ ! -e /nix/store-init/.nix-seeded ]; then \
  cp -a /nix/store/. /nix/store-init/ && \
  cp -a /nix/var/nix/. /nix/var-init/ && \
  touch /nix/store-init/.nix-seeded; \
  fi

COPY . /tmp/build
WORKDIR /tmp/build

# The caches now contain the base image's store content plus any previously
# compiled derivations, so Nix can reuse them instead of rebuilding from scratch.
# The closure copy and result dereference must happen in this same RUN step
# while the cache mounts are still active.
RUN --mount=type=cache,id=jellyswarrm-nix-store,target=/nix/store,sharing=locked \
  --mount=type=cache,id=jellyswarrm-nix-var,target=/nix/var/nix,sharing=locked \
  nix --extra-experimental-features "nix-command flakes" \
  --option filter-syscalls false \
  build path:.#jellyswarrm --print-build-logs --verbose && \
  mkdir -p /tmp/nix-store-closure /tmp/build-result && \
  cp -R $(nix-store -qR result/) /tmp/nix-store-closure && \
  cp -rL result/. /tmp/build-result/


#################################
# Stage 2: Minimal scratch image
#################################
FROM scratch

WORKDIR /app

# The closure provides glibc, sqlite, and cacert — everything the binary needs.
COPY --from=builder /tmp/nix-store-closure /nix/store
COPY --from=builder /tmp/build-result /app

ENV RUST_LOG=info

EXPOSE 3000

ENTRYPOINT ["/app/bin/jellyswarrm-proxy"]
