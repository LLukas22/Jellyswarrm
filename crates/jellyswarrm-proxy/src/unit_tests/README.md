# External unit and component tests

Large test modules live here, named for the component they exercise. Each owning
source module includes its file with `#[cfg(test)]` and `#[path = "..."]`.
This preserves private access, `super::` imports, and existing test filters.
The owning modules are declared by `src/lib.rs`, so use the library test target.

Owners now include `auth/service/mod.rs`, `libraries/service/mod.rs`,
`media/storage/mod.rs`, `http/preprocessing.rs`, and `http/client_routes.rs`.
Paths are relative to the owning source file, not this directory. Keep these
suites attached to their owners when splitting implementation modules; do not
broaden visibility merely to make tests compile. Playlist contracts live in
`../http_tests/playlists.rs` and share production proxy/item-detail helpers.

- Keep small unit tests inline with their implementation.
- Put larger extracted suites here instead of creating a folder for one `tests.rs`.
- HTTP workflows belong in `../http_tests/`.
- Model fixtures stay in `../models/tests/files/`; session/WebSocket HTTP tests are
  in `../http_tests/sessions.rs`.

Run all proxy tests from the repository root:

```sh
cargo test --locked -p jellyswarrm-proxy --lib
```
