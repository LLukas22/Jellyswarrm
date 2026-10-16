# Web-client HTTP contracts

This suite tests the boundary that the web client actually uses:

```text
client HTTP request → production Axum routes → identity/routing/processors
                    → independently observable mock Jellyfin backends
                    → client-facing status, DTO, headers and bytes
```

It does **not** call handlers directly or duplicate a simplified media router.
`src/client_api_routes.rs` is shared by the running application and this fixture.
The fixture also includes the production session router, generic proxy fallback,
URL prefix, and read-only API-key middleware. Browser management-login/session
middleware and UI pages are not exercised here.

## Run

From the repository root, without Docker, a browser, or a UI build:

```sh
just http-test
# Run just one configuration:
just http-test-normal
just http-test-merged
# equivalent:
JELLYSWARRM_SKIP_UI=1 cargo test -p jellyswarrm-proxy --bin jellyswarrm-proxy http_tests

# One route or workflow:
JELLYSWARRM_SKIP_UI=1 cargo test -p jellyswarrm-proxy --bin jellyswarrm-proxy http_tests::catalog::episodes
JELLYSWARRM_SKIP_UI=1 cargo test -p jellyswarrm-proxy --bin jellyswarrm-proxy http_tests::pipeline::regressions::web_client

# Print the inventory's coverage classifications:
JELLYSWARRM_SKIP_UI=1 cargo test -p jellyswarrm-proxy --bin jellyswarrm-proxy http_tests::coverage -- --nocapture
```

Each test owns an in-memory SQLite database, two Wiremock servers, a proxy on an
ephemeral local port, mapped backend credentials for Alice, and an unmapped Bob.
No running development server or credentials are required. Unexpected backend
requests receive 404; expectation counts and empty-other-backend assertions make
wrong destinations observable. Fixtures abort their proxy task on drop.

## Two explicit scenarios

Shared contracts in `access`, `bodies`, `catalog`, `discovery`, `editing`,
`hls`, `live_tv`, `mutations`, `preferences` and `resources` each run as two independently
named tests. The fixture sets these flags explicitly, rather than inheriting
application defaults:

| Test suffix | `merge_libraries` | `deduplicate_media` | Expected presentation |
| --- | --- | --- | --- |
| `normal_servers` | false | false | Separate libraries and source-owned media from each connected server |
| `merged_libraries` | true | true | Matching libraries grouped; provider-matched movies/episodes expose selectable versions |

Library merging and media deduplication are separate settings. These are the two
end-to-end configurations under test, not every possible combination. Audio does
not participate in media deduplication: a merged music library still lists both
backend tracks. Library grouping requires a matching name and collection type.

For example, `http_tests::catalog::modern_detail::normal_servers` and
`http_tests::catalog::modern_detail::merged_libraries` execute the same contract
with independent databases, mock servers and proxy instances. Use `scenario_test!`
to register a shared contract once for both configurations.

`pipeline/media_paths.rs` runs movies, episodes and audio through both scenarios
and both proxy/redirect delivery modes:

```text
GET UserViews → GET Items?ParentId=<returned library>
              → GET Items/<returned item> (metadata, chapters, tracks, versions)
              → POST PlaybackInfo (select each advertised source)
              → ranged direct stream + negotiated HLS master → variant → segment
              → Playing → Progress → Stopped
```

These workflows obtain library, item and source identities from HTTP responses;
they do not pre-create groups or media/source mappings in the database. They
verify both backend copies, exact upstream routing, translated credentials and
IDs, preserved stream bytes/headers, session ownership, and rejection of foreign
viewers without upstream traffic. Redirect cases check the 307 and follow its
Location without supplying backend credentials themselves.

HLS requests use only the negotiated URL and playlist references: no test-side
Authorization header or appended session query. Children resolve against the
final response URL after redirects. Proxy manifests must replace backend tokens
and source IDs; Redirect manifests intentionally remain backend-owned. Dedicated
`hls` cases cover URI attributes (audio, keys, initialization maps), relative,
root-relative, absolute and protocol-relative references, external-origin
preservation, query escaping, cache/partial-response safety and invalid upstream
responses. Foreign-viewer denials require `403`, including a user mapped to one
backend attempting to read, stream or create recordings from the other backend.

Focused `pipeline` regressions retain their specific configuration setup (for
example configured libraries, deduplication without automatic library merging,
and outages). `just http-test` runs those and the inventory checks as well as both
scenario matrices. The scenario-specific commands select only the matrix cases.

## Separate from real integration tests

These are deterministic Rust HTTP contract/workflow tests with mock upstreams,
not the Docker-backed tests in `tests/server_integration.rs` or browser tests in
`tests/browser/`. They transfer and assert media bytes but do not transcode or
decode real media. `just integration-test` and the `browser-integration-test*`
commands remain the independent real-server/browser checks; none is required by
the HTTP suite.

## Modules

| Module | Contract |
| --- | --- |
| `support.rs` | Shared real HTTP/SQLite fixture and identity helpers |
| `discovery.rs` | Library roots, search, global catalogs, named categories, Live TV fan-out |
| `catalog.rs` | Modern/legacy item reads, details, feeds, shows, facets, playlists, media segments, lyrics, recommendations |
| `mutations.rs` | Watched/favorite actions, queue/container query IDs, timer cancellation |
| `bodies.rs` | Playlist/collection creation, sharing, recording edits, recursive JSON ID translation |
| `resources.rs` | Video/audio, HLS paths, subtitle/attachment paths, trickplay, images, downloads, fonts, range bytes/headers |
| `hls.rs` | Player-style manifest URL/URI-attribute following, credential isolation, cache/range safety and invalid upstream responses |
| `preferences.rs` | Client-owned display settings and user-ID translation |
| `editing.rs` | Metadata/image/lyric editing transport contracts |
| `live_tv.rs` | POST guide fan-out, per-backend channel filters, body pagination, recording identities |
| `access.rs` | Foreign viewers, upstream errors, auth forms, UUID/case variants, HEAD resources |
| `pipeline/` | Stateful workflows and migrated permanent HTTP regressions: merged libraries, inventories/outages, queue-to-negotiation, source selection, progress/stop, resume filtering, live opening/refresh/close, cancellation, proxy/redirect delivery |
| `pipeline/media_paths.rs` | Two-scenario discovery-to-information-to-streaming matrix for movies, episodes and audio, with proxy and redirect delivery |
| `coverage.rs` | Classification of discovered routes, workflow references and dynamic dispatch families |

The migrated tests are no longer nested under `sessions::tests`; their names now
start with `http_tests::pipeline`. Local session/WebSocket tests remain under
`sessions::tests` and reuse the same fixture.

## Inventory and drift detection

`scripts/scan-client-routes.cjs` walks `ui/src` using the TypeScript AST. It resolves
SDK operations through the installed generated API, legacy `ApiClient` wrappers
through their source map, URL helpers, and direct `getUrl` expressions. The
committed `client_routes.json` records file/line/operation provenance plus SDK
query/body capabilities. Computed URLs and method dispatch are listed separately.

Install the checked-in UI dependencies before scanning (`npm ci` in `ui`), then:

```sh
just http-routes-update    # regenerate after a web-client/SDK change
just http-routes-check     # fail if the committed inventory is stale
```

CI runs the freshness check after installing UI dependencies. Rust tests consume
the committed inventory, so running the HTTP suite itself needs no Node install.
The coverage test rejects discovered routes without one of:

1. An executable request contract.
2. An executable stateful workflow contract.
3. An explicit, endpoint-specific scope exclusion.

The initial scan records **397 resolved call sites and 65 dynamic sites**. These
are call sites, not 397 unique endpoints or complete runtime coverage. `CONTEXT`
entries identify URL construction whose verb requires manual review. Dynamic
media URLs are covered by resource contracts and negotiated-delivery workflows;
editor/admin and user-image dispatches have explicit exclusions.

This is a conservative static inventory, not whole-program analysis. It cannot
enumerate every URL a backend may return, every alias/import transformation, or
every combination of query fields, media types and client branches. Transport
contracts establish representative request/response shapes; workflows establish
important multi-request invariants. Real Jellyfin/browser integration remains
necessary for backend DTO semantics, codec negotiation and actual decoding.

## Former known failures now run normally

The seven originally ignored contracts have been fixed and moved into the regular
modules. The HTTP suite now has **no ignored contracts**:

| Contract | Fix |
| --- | --- |
| Additional video parts return virtual IDs | Explicit JSON item-list route instead of streaming wildcard |
| Legacy POST guide request | Registered POST with the shared catalog policy and typed upstream body |
| Recording creation | `ProgramId` body routing and translation |
| Series-recording creation | Same `ProgramId` body handling |
| Recording defaults follow the program owner | `ProgramId` query routing and response translation |
| Metadata POST reaches the item owner | POST registered on the item method router |
| Remote metadata apply reaches the item owner | Complete deep-path tag used for ownership and translation |

POST guide tests also cover unscoped fan-out, channel-list partitioning, advancing
upstream body pages, global client pagination, malformed DTOs and foreign viewers.
The POST DTO is authoritative over matching query fields; unrelated query fields
survive. Upstream requests keep the POST method and JSON field types.

Never replace a desired contract with an assertion that a broken status or raw ID
is correct. If a new defect is found, retain a labelled runnable reproduction and
document the coverage gap explicitly rather than silently excluding its route.

## Add a contract

1. Find the client call and its SDK/legacy wire shape; regenerate the inventory.
2. Add a named case to the matching module. Specify realistic client query/body
   fields and response shape. Pin the original backend ID, user/token, method,
   relevant query/body data and request count.
   Use `Fixture::for_scenario` and `scenario_test!` for contracts shared by the two
   scenarios; make different expected results explicit instead of overriding the
   scenario's configuration inside the test.
3. Assert returned virtual IDs, result shape, preserved settings or binary bytes,
   and that unrelated backends received no requests.
4. For playback/stateful features, add a sequence to `pipeline/` rather than only
   an isolated successful request. Include explicit-source, foreign-user,
   unavailable-owner or refresh/retry cases where applicable.
5. Register the contract in the coverage inventory; no blanket route-prefix
   exclusions. If the desired behavior currently fails, retain a labelled gap
   reproduction with the actual failure documented here.

Keep backend mocks strict. Returning 200 for every method/path would hide exactly
the routing and translation defects this suite is intended to detect.
