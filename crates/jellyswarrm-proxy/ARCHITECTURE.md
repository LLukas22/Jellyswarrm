# Proxy architecture

Jellyswarrm presents several Jellyfin servers as one. Its modules separate **how
clients connect**, **what an operation should do**, and **how data is stored,
translated or fetched**. This keeps HTTP details out of service logic and lets
workflows be tested without starting a server.

All module paths below are relative to `src/`.

## Startup and incoming requests

| Module | Purpose |
|---|---|
| `app/` | Starts the process: loads configuration, constructs services, assembles both routers and owns background tasks and shutdown. |
| `http/` | Handles the client API, authentication extraction, WebSocket connections, embedded web assets and HTTP responses. `http/state.rs` holds the focused service handles handlers need. |
| `ui/` | Serves the management pages, forms and UI login. It is separate from the client API because its presentation and authentication needs differ. |
| `application/` | Coordinates catalog, playback, account and administration operations. It decides which backends a caller may use and combines the services needed to complete a request. |

`main.rs` supplies the Tokio runtime and calls the library's only public entry
point, `run()`. Startup wires the modules together; request handling never calls
back into startup.

## Feature services

These modules own the rules and state of a particular feature, rather than an
entire HTTP request:

| Module | Purpose |
|---|---|
| `auth/` | Local users, credentials, API keys, backend account mappings and Quick Connect. |
| `servers/` | Backend registration, addresses, health and stable server ordering. |
| `libraries/` | Virtual library groups and their backend members. |
| `media/` | Media identity, virtual ID mappings, versions and catalog inventory. |
| `playback/` | Playback-session and stream bindings that keep later requests on the right backend. |
| `sessions/` | Connected client presence, observed playback state and bounded message delivery. |
| `syncplay/` | Group membership, shared queues and synchronized playback control. |

Playback bindings and client sessions are separate: one tracks **where media is
playing**, the other tracks **which clients are connected**. Cross-feature
authorization and coordination belong in `application/`; HTTP parsing and
WebSocket frames belong in `http/`.

## Translation, transport and storage

| Module | Purpose |
|---|---|
| `processors/` | Inspects and translates JSON, URLs and identifiers between the virtual server and a selected backend, including JSON body framing. |
| `upstream/` | Makes backend requests and delivers JSON, streams and HLS playlists. |
| `persistence/` | Opens SQLite, runs migrations and converts shared values to database representations. SQL also still lives in feature services. |
| `config.rs`, `config/` | Loads configuration, provides startup and runtime settings, and serializes save/reload operations. |

Changing an identifier is not the same responsibility as sending a request, so
translation does not depend on transport. Transport uses translation in one place:
HLS playlist rewriting maps each playlist URL through `UrlProcessor`. Application
workflows combine the two.
Handlers read runtime settings through `SettingsView`; configuration changes go
through `ConfigurationService`, not a writable configuration lock.

## Shared vocabulary

| Module | Purpose |
|---|---|
| `domain/` | Shared IDs and security values, independent of HTTP and database libraries. |
| `models/` | Jellyfin request and response data shapes, including compatibility fields. |
| `protocol/` | Pure authorization/device parsing and Jellyfin route and URL conventions. |

Keeping these definitions separate gives each shared concept one owner without
requiring callers to import a feature service just to use its types.

## Following a request

A catalog request enters through `http/`, becomes an application operation, then
uses feature services, processors and upstream transport to produce a response.
A remote-control request instead uses application session checks, SyncPlay's
group rules and session delivery before the WebSocket adapter sends the message.
See [request and data flow](src/DATA_FLOW.md) for concrete paths.

Dependencies are one-way **between these top-level module owners**, not necessarily
between every internal file.
