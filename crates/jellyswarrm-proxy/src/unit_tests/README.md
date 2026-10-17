# External unit and component tests

Large test modules live here, named for the component they exercise. Each owning
source module includes its file with `#[cfg(test)]` and `#[path = "..."]`.
This preserves private access, `super::` imports, and existing test filters.

Owners now include `auth/service/mod.rs`, `libraries/service/mod.rs`,
`media/storage/mod.rs`, `http/preprocessing/mod.rs`, and `http/handlers/`.
Paths are relative to the owning source file, not this directory. Keep these
suites attached to their owners when splitting implementation modules; do not
broaden visibility merely to make tests compile. Playlist contracts live in
`../http_tests/playlists.rs` and share production proxy/item-detail helpers.

- Keep small unit tests inline with their implementation.
- Put larger extracted suites here instead of creating a folder for one `tests.rs`.
- HTTP workflows belong in `../http_tests/`.
- Model fixtures stay in `../models/tests/`; session/WebSocket tests stay in
  `../sessions/tests.rs`.

Run all proxy tests from the repository root:

```sh
JELLYSWARRM_SKIP_UI=1 cargo test -p jellyswarrm-proxy --bin jellyswarrm-proxy
```
