set shell := ["bash", "-uc"]

compose := "docker compose --file dev/docker-compose.yml"

# List the available local development commands.
default:
    @just --list

# Verify that Docker and Docker Compose are usable.
doctor:
    @command -v docker >/dev/null || { printf 'Docker is required.\n' >&2; exit 1; }
    @docker compose version >/dev/null || { printf 'Docker Compose v2 is required.\n' >&2; exit 1; }
    @docker info >/dev/null || { printf 'The Docker daemon is not available.\n' >&2; exit 1; }
    @printf 'Docker is ready.\n'

# Fetch LFS media, start six Jellyfin servers, initialize them, and start Caddy.
setup: media up

# Start the complete development media-server stack and wait until it is ready.
up: doctor
    {{compose}} up --detach --remove-orphans --wait --wait-timeout 1800 caddy
    @just urls

# Stop and remove the development containers while preserving all data.
down:
    {{compose}} down --remove-orphans

# Stop the development containers without removing them.
stop:
    {{compose}} stop

# Restart the complete development media-server stack.
restart:
    just down
    just up

# Fetch the compact media fixtures tracked by Git LFS.
media:
    @command -v git-lfs >/dev/null || { printf 'Git LFS is required.\n' >&2; exit 1; }
    git lfs pull --include="dev/media/**"

# Pull the latest images used by the development stack.
pull: doctor
    {{compose}} pull

# Show container and health status.
status:
    {{compose}} ps --all

# Follow logs for the entire development stack.
logs:
    {{compose}} logs --follow --tail 100

# Follow logs for one Compose service, for example: just log jellyfin-movies-2
log service:
    {{compose}} logs --follow --tail 100 {{service}}

# Validate the Compose file and Python initializer.
check: doctor
    {{compose}} config --quiet
    @docker run --rm --volume "$PWD/dev/scripts:/scripts:ro" ghcr.io/astral-sh/uv:python3.12-alpine python -c 'from pathlib import Path; [compile(path.read_text(), str(path), "exec") for path in Path("/scripts").glob("*.py")]'
    @printf 'Development configuration is valid.\n'

# Remove only Jellyfin configuration/cache, then recreate the stack.
reset:
    {{compose}} down --remove-orphans
    rm --recursive --force "dev/data/jellyfin-movies" "dev/data/jellyfin-tvshows" "dev/data/jellyfin-music" "dev/data/jellyfin-movies-2" "dev/data/jellyfin-tvshows-2" "dev/data/jellyfin-music-2"
    just up

# Print direct server URLs and the Caddy entry point.
urls:
    @printf '%-10s %s\n' 'Movies 1' 'http://localhost:8096' 'Shows 1' 'http://localhost:8097' 'Music 1' 'http://localhost:8098' 'Movies 2' 'http://localhost:8099' 'Shows 2' 'http://localhost:8100' 'Music 2' 'http://localhost:8101' 'Caddy' 'http://localhost:8000'
