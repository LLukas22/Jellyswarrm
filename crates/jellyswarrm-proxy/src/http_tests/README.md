# HTTP contract tests

```text
Client request → production Axum routes → mock Jellyfin backends → response assertions
```

Each test uses an in-memory SQLite database, two Wiremock backends, and a proxy on
an ephemeral port. No Docker, browser, UI build, or external credentials required.
Real-server and browser tests live separately in `tests/`.

## Run

From the repository root:

```sh
just http-test          # all contracts, workflows, and coverage checks
just http-test-normal  # separate libraries and source-owned media
just http-test-merged  # merged libraries and deduplicated media

# One test or module:
JELLYSWARRM_SKIP_UI=1 cargo test -p jellyswarrm-proxy --bin jellyswarrm-proxy http_tests::catalog::episodes
```

Shared contracts use `scenario_test!` to run both configurations. Focused pipeline
regressions may use other settings; the scenario-specific commands exclude those.

## Where tests belong

| Module | Coverage |
|---|---|
| `support.rs` | HTTP/SQLite fixtures and identity helpers |
| `discovery.rs`, `catalog.rs` | Libraries, search, item details, feeds, facets |
| `mutations.rs`, `bodies.rs` | State changes and JSON ID translation |
| `resources.rs`, `hls.rs` | Streaming, playlists, subtitles, images, ranges |
| `preferences.rs`, `editing.rs` | Client settings and metadata edits |
| `live_tv.rs` | Guides, channel filters, recording identities |
| `access.rs` | Authorization, errors, identity variants |
| `pipeline/` | Stateful playback, library, outage, and live-stream workflows |
| `pipeline/media_paths.rs` | Discovery → detail → negotiation → delivery → playback reports |
| `coverage.rs` | Client-route inventory classifications |

## Route inventory

After changing client calls or the SDK, install UI dependencies with `npm ci` in
`ui`, then run from the repository root:

```sh
just http-routes-update
just http-routes-check
```

`scripts/scan-client-routes.cjs` generates `client_routes.json`. Each discovered
route needs a request contract, workflow contract, or endpoint-specific exclusion.
The inventory is static coverage guidance, not proof of every runtime path.
Rust tests read the committed inventory and do not require Node.

## Add a contract

1. Match the client's real method, query/body fields, and response shape.
2. Use `Fixture::for_scenario` and `scenario_test!` for shared contracts.
3. Assert exact upstream IDs, credentials, request counts, and destination;
   verify client-facing IDs, DTOs, headers, or bytes.
4. Put multi-request behavior in `pipeline/`, using identities returned by earlier
   responses. Include relevant authorization, outage, or retry cases.
5. Update coverage registration. Keep mocks strict and retain runnable defect
   reproductions rather than accepting broken behavior or broadly excluding routes.
