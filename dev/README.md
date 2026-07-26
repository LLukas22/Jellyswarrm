# Jellyfin Development Environment

A Docker Compose environment for testing Jellyswarrm against two independent
Jellyfin servers for each library type: Movies, TV Shows, and Music. All six
servers use read-only trees of compact, freely licensed test media while keeping
their configuration, users, item databases, and caches isolated.

## Quick start

From the repository root:

```bash
just setup
```

This command verifies Docker, fetches the Git LFS media, starts all six servers,
initializes their users and libraries, and waits for the complete stack to be
ready. Media and server state are reused on later runs.

Debug builds of `jellyswarrm-proxy` load `data/jellyswarrm.dev.toml` and
automatically add or update these six servers in Jellyswarrm. Existing
non-development servers are left untouched. They also create the local
Jellyswarrm user `test` / `test` and map all six upstream servers to that user
with the same credentials.

Requirements:

- Docker with the Compose v2 plugin
- Git LFS
- [just](https://just.systems/)

The exact Jellyfin release is set once through `JELLYFIN_VERSION` in
[`dev/.env`](.env). All six servers use that version; update the value and run
`just pull && just up` to test another release.

## Servers

| Server | Direct URL |
| --- | --- |
| Movies 1 | <http://localhost:8096> |
| Shows 1 | <http://localhost:8097> |
| Music 1 | <http://localhost:8098> |
| Movies 2 | <http://localhost:8099> |
| Shows 2 | <http://localhost:8100> |
| Music 2 | <http://localhost:8101> |

Every server has the regular Jellyswarrm account `test` / `test`, with access
to its complete library. The administrator account remains `admin` / `password`.

Caddy is available at <http://localhost:8000> and exposes `/movies/`,
`/shows/`, `/music/`, `/movies-2/`, `/shows-2/`, and `/music-2/`.

## Commands

Run `just` to list all commands. Common workflows are:

```bash
just up       # Start and wait for the complete stack
just down     # Remove containers but preserve media and server state
just status   # Show all containers, including one-shot initializers
just logs     # Follow logs from the complete stack
just media    # Fetch media fixtures from Git LFS
just check    # Validate Compose and the initializer
just reset    # Recreate all server state but preserve tracked media
```

Use `just log jellyfin-movies-2` to follow one service.

## Integration tests

Run the Docker-backed end-to-end test with:

```bash
just integration-test
```

The Rust test uses Testcontainers' Docker Compose support with
`docker-compose.yml` and `docker-compose.integration.yml`. It creates a unique
Compose project with fresh Jellyfin configuration volumes and random host
ports, so it can run while the regular development stack is active. The test
starts the compiled Jellyswarrm server against those six instances and checks
failed and successful login, automatic user mappings, merged virtual
libraries, duplicate source labeling, playback info, and ranged media streaming
from both movie servers. The test is ignored by normal `cargo test` runs
because initializing all six media servers is intentionally heavyweight.

## Media layout

Fixtures are stored under `dev/media/` through Git LFS:

```text
dev/media/
|-- movies/
|   |-- server-1/
|   `-- server-2/
|-- tv-shows/
|   |-- server-1/
|   `-- server-2/
`-- music/
    |-- server-1/
    `-- server-2/
```

Each directory is mounted only into its matching Jellyfin server:

| Server | Fixtures |
| --- | --- |
| Movies 1 | Night of the Living Dead (exclusive), Big Buck Bunny (shared) |
| Movies 2 | Plan 9 from Outer Space and Sintel (exclusive), Big Buck Bunny (shared) |
| Shows 1 | The Cisco Kid (exclusive); One Step Beyond S01E02 (shared) and S02E21 (exclusive) |
| Shows 2 | One Step Beyond S01E02 (shared) and S03E34 (exclusive) |
| Music 1 | The Open Goldberg Variations |
| Music 2 | Ghost Solos |

The One Step Beyond layout provides one season and episode shared by both
Shows servers, plus a different server-specific season on each. Shared files
have identical LFS object IDs, so GitHub stores each shared binary once. This
gives merged-library tests both overlapping and exclusive upstream items.

Videos are two-minute excerpts of the real public-domain and Creative Commons
works, compacted to a maximum width of 640px. The freely licensed music tracks
remain complete. The whole fixture set is approximately 52 MiB. See
[`MEDIA-LICENSES.md`](MEDIA-LICENSES.md) for exact sources, licenses, and
required attribution.

Jellyfin state is stored in `dev/data/jellyfin-*`. `just down` preserves it;
`just reset` removes only this state and runs initialization again. Tracked
media is preserved by both commands.
