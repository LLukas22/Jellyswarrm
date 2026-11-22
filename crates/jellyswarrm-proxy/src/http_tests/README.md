# HTTP contract tests

```text
Client request → production Axum routes → mock Jellyfin backends → response assertions
```

Each test uses an in-memory SQLite database, two Wiremock backends, and a proxy on
an ephemeral port. No Docker, browser, UI build, or external credentials required.
Real-server and browser tests live separately in `tests/`.

These source-owned contracts run under the proxy library's test target. The
thin executable does not compile a second copy of the application test modules.

## Run

From the repository root:

```sh
just http-test          # all contracts, workflows, and coverage checks
just http-test-normal  # configuration-dependent contracts: separate libraries
just http-test-merged  # configuration-dependent contracts: merged libraries

# One test or module:
cargo test --locked -p jellyswarrm-proxy --lib http_tests::catalog::episodes
```

Source-owned transport contracts use `contract_test!` and run once, with separate
servers. Use `scenario_test!` to run both configurations only when the fixture
exercises merging, aggregate identities, or configuration-dependent behavior.
Library-root discovery, search without provider IDs, and end-to-end media workflows
retain both configurations.
The scenario-specific commands exclude single-run contracts and focused
regressions; use `just http-test` for the complete HTTP suite.

## Where tests belong

| Module | Coverage |
|---|---|
| `support.rs` | HTTP/SQLite fixtures and identity helpers |
| `discovery.rs`, `catalog.rs` | Libraries, search, item details, feeds, facets |
| `mutations.rs`, `bodies.rs` | State changes and JSON ID translation |
| `resources.rs`, `hls.rs` | Streaming, playlists, subtitles, images, ranges |
| `preferences.rs`, `editing.rs` | Client settings and metadata edits |
| `live_tv.rs` | Guides, channel filters, recording identities |
| `client_auth.rs` | Client-specific password and Quick Connect requests, saved-token access, and logout |
| `kodi.rs` | Kodi Quick Connect → sync → placeholder-device playback; token-owner isolation, expiry, and KodiSyncQueue IDs |
| `access.rs` | Authorization, errors, identity variants |
| `pipeline/` | Stateful playback, library, outage, and live-stream workflows |
| `pipeline/media_paths.rs` | Discovery → detail → negotiation → delivery → playback reports |
| `pipeline/regressions/episode_queue.rs` | Unmodified Jellyfin Web episode navigation, bounded queue windows, alternate-version safety, and unchanged ordinary `StartItemId` semantics |
| `coverage.rs` | Client-route inventory classifications |

## Client authentication

Run `cargo test --locked -p jellyswarrm-proxy --lib http_tests::client_auth` to check these flows independently. Each client signs in by password and by Quick Connect, then uses the returned token from a fresh HTTP client to read its identity and libraries. The library mocks require the newly issued upstream token. After logout, the same proxy token must receive HTTP 401.

The tests use the request formats from the source revisions checked on 2026-11-22. CI runs them offline. Recheck these call sites when updating client compatibility. The embedded [login controller](../../../../ui/src/apps/legacy/controllers/session/login/index.js) and its pinned apiclient have a separate test because their request casing differs from upstream Jellyfin Web. Desktop and the mobile WebView wrappers share the embedded web login. Browser tests cover UI interaction.

| Client source | Behavior covered |
|---|---|
| [Jellyfin Web](https://github.com/jellyfin/jellyfin-web/blob/5481eb1f1582cf9eb708315050e93f933ffee64b/src/apps/legacy/controllers/session/login/index.js) | Password and Quick Connect through the TypeScript SDK. |
| [Jellyfin Desktop](https://github.com/jellyfin/jellyfin-desktop/blob/70930139f7ef0c59b6ac98e8d0be436953c79083/native/find-webclient.js) | Connects to the server web client. |
| [Jellyfin for Android](https://github.com/jellyfin/jellyfin-android/blob/fcf4b8aaafe87e3aaaa03a5273f1d13354729ff3/app/src/main/java/org/jellyfin/mobile/webapp/WebViewFragment.kt) | Loads the server web client in a WebView. |
| [Jellyfin for iOS](https://github.com/jellyfin/jellyfin-expo/blob/11fbb2ac762b2d7a61fc8e36f14bfbb4fec95685/components/NativeShellWebView.tsx) | Hosts the server web client with native shell integration. |
| [Android TV / Fire TV](https://github.com/jellyfin/jellyfin-androidtv/blob/dc49f2eb0080803668fb5c3ba0fddcccec397c10/app/src/main/java/org/jellyfin/androidtv/auth/repository/AuthenticationRepository.kt) | Password login uses a username-derived device ID. Quick Connect starts with the installation ID. The saved session uses a user-ID-derived device ID. [Session switch](https://github.com/jellyfin/jellyfin-androidtv/blob/dc49f2eb0080803668fb5c3ba0fddcccec397c10/app/src/main/java/org/jellyfin/androidtv/auth/repository/SessionRepository.kt) |
| [Findroid](https://github.com/jarnedemeulemeester/findroid/blob/c7a8b0b200a07f8588c5251e95b8d0308a18641e/setup/src/main/java/dev/jdtech/jellyfin/setup/data/SetupRepositoryImpl.kt) | Saves the server ID, user ID, username and token returned by the Kotlin SDK. |
| [Streamyfin](https://github.com/streamyfin/streamyfin/blob/68af07d44394510b32172a65b07dd1d57a6b7295/providers/JellyfinProvider.tsx) | SDK password login and a separate Quick Connect flow. Polls with `Secret` and redeems with `secret`. [Quick Connect](https://github.com/streamyfin/streamyfin/blob/68af07d44394510b32172a65b07dd1d57a6b7295/hooks/useQuickConnect.ts) |
| [Swiftfin](https://github.com/jellyfin/Swiftfin/blob/db3ae5c97bfbe824bc9581fc5539a167951f6c77/Shared/ViewModels/UserSignInViewModel.swift) | Requires a token, user ID and username. Its Swift SDK sends unquoted authorization parameters. [SDK header](https://github.com/jellyfin/jellyfin-sdk-swift/blob/e69dd0358ad33148aded867661e27d71de29d0c3/Sources/PassthroughAPIClientDelegate.swift) |
| [Finamp](https://github.com/jmshrv/finamp/blob/1ee9e676f21c135f9f6cb32eac59e82bf779864d/lib/services/jellyfin_api_helper.dart) | Polls and redeems with `Secret`. Authenticated requests include `UserId` and `Token` before the client fields. [API and headers](https://github.com/jmshrv/finamp/blob/1ee9e676f21c135f9f6cb32eac59e82bf779864d/lib/services/jellyfin_api.dart) |
| [Jellyfin for Roku](https://github.com/jellyfin/jellyfin-roku/blob/d09949789c1e615c0aa75699512ff77980a7fbdc/source/api/userauth.bs) | Password login requests `?format=json`. Quick Connect uses lowercase `secret` and the saved session reads `/Users/{id}`. |
| [Jellyfin for Kodi](https://github.com/jellyfin/jellyfin-kodi/blob/7d74d62ec0c19d4cd311419f20162049df0c559b/jellyfin_kodi/jellyfin/api.py) | Bundled Python client uses lowercase `username`, unquoted encoded headers and `/system/info` for saved-token validation. |
| [Jellyfin MPV Shim](https://github.com/jellyfin/jellyfin-mpv-shim/blob/a2e8c1988ab23cbd4ed78d9d8114b92fb589e70b/jellyfin_mpv_shim/clients.py) | Uses the Python API client, opens an authenticated WebSocket, and posts capabilities. [Socket handshake](https://github.com/jellyfin/jellyfin-apiclient-python/blob/d6a33783f926eaa6fed5d09bd9639604ce055242/jellyfin_apiclient_python/ws_client.py) |

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
2. Use `contract_test!` for source-owned transport contracts. Opt into
   `Fixture::for_scenario` and `scenario_test!` when configuration changes the
   behavior under test, rather than multiplying every route by configuration.
3. Assert exact upstream IDs, credentials, request counts, and destination;
   verify client-facing IDs, DTOs, headers, or bytes.
4. Put multi-request behavior in `pipeline/`, using identities returned by earlier
   responses. Include relevant authorization, outage, or retry cases.
5. Update coverage registration. Keep mocks strict and retain runnable defect
   reproductions rather than accepting broken behavior or broadly excluding routes.
