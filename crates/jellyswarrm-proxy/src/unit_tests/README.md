# External unit and component tests

Large test modules live here, named for the component they exercise. Each owning
source module includes its file with `#[cfg(test)]` and `#[path = "..."]`.
This preserves private access, `super::` imports, and existing test filters.

- Keep small unit tests inline with their implementation.
- Put larger extracted suites here instead of creating a folder for one `tests.rs`.
- HTTP workflows belong in `../http_tests/`.
- Model fixtures stay in `../models/tests/`; session/WebSocket tests stay in
  `../sessions/tests.rs`.

Run all proxy tests from the repository root:

```sh
JELLYSWARRM_SKIP_UI=1 cargo test -p jellyswarrm-proxy --bin jellyswarrm-proxy
```
