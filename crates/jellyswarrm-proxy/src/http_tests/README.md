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

## Modules

| Module | Contract |
| --- | --- |
| `support.rs` | Shared real HTTP/SQLite fixture and identity helpers |
| `discovery.rs` | Library roots, search, global catalogs, named categories, Live TV fan-out |
| `catalog.rs` | Modern/legacy item reads, details, feeds, shows, facets, playlists, media segments, lyrics, recommendations |
| `mutations.rs` | Watched/favorite actions, queue/container query IDs, timer cancellation |
| `bodies.rs` | Playlist/collection creation, sharing, recording edits, recursive JSON ID translation |
| `resources.rs` | Video/audio, HLS paths, subtitle/attachment paths, trickplay, images, downloads, fonts, range bytes/headers |
| `preferences.rs` | Client-owned display settings and user-ID translation |
| `editing.rs` | Metadata/image/lyric editing transport contracts |
| `access.rs` | Foreign viewers, upstream errors, auth forms, UUID/case variants, HEAD resources |
| `pipeline/` | Stateful workflows and migrated permanent HTTP regressions: merged libraries, inventories/outages, queue-to-negotiation, source selection, progress/stop, resume filtering, live opening/refresh/close, cancellation, proxy/redirect delivery |
| `coverage.rs` | Classification of discovered routes, workflow/gap references and dynamic dispatch families |
| `known_gaps.rs` | Desired contracts that currently reproduce production defects |

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
3. An executable known-gap reproduction.
4. An explicit, endpoint-specific scope exclusion.

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

## Known failures are visible, not accepted

`known_gaps.rs` uses `#[ignore = "specific production defect"]` so existing
contracts remain runnable while the backlog is explicit. All seven desired
contracts were run and failed at the time this suite was introduced:

| Desired contract | Observed failure |
| --- | --- |
| Additional video parts return virtual IDs | Raw backend IDs reach the client |
| Legacy POST guide request | `/LiveTv/Programs` returns 405 before forwarding |
| Recording creation | `ProgramId` remains virtual in upstream JSON |
| Series-recording creation | Same missing `ProgramId` JSON translation |
| Recording defaults follow the program owner | `ProgramId` query routing/translation missing; 404 |
| Metadata POST reaches the item owner | Item method router returns 405 |
| Remote metadata apply reaches the item owner | Deep path ID not routed/translated; 404 |

Run this backlog explicitly (it is expected to return a nonzero exit status):

```sh
just http-test-gaps
```

When fixing a gap, remove its ignore marker and move the test/coverage entry into
the regular module. Never replace a desired contract with an assertion that the
broken status or raw ID is correct. A passing default suite is **not** a claim
that these seven defects are fixed.

## Add a contract

1. Find the client call and its SDK/legacy wire shape; regenerate the inventory.
2. Add a named case to the matching module. Specify realistic client query/body
   fields and response shape. Pin the original backend ID, user/token, method,
   relevant query/body data and request count.
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
