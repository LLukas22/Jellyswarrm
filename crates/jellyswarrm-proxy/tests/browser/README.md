# Browser integration tests

These verify remote control across two media backends and two-client SyncPlay
through Jellyfin Web: create/join a group, play, pause, seek, switch media across backends, leave, and rejoin during playback.

The Rust Playwright tests share the server integration fixture and browser setup. Each Chrome
instance runs in its own container; Testcontainers manages startup and cleanup.

Run on Linux x86-64 with Docker, Git LFS, and the usual Rust/UI build prerequisites:

```sh
just browser-integration-test-docker
```

CI runs the same command. Videos (`controller.webm`, `receiver.webm`), traces,
and failure screenshots go to `target/browser-test-artifacts/remote-control/`
and `target/browser-test-artifacts/syncplay/` and are uploaded
as `browser-test-artifacts` for seven days, including on test failure.

For native Chrome debugging, use `just browser-integration-test` with
`JELLYSWARRM_BROWSER_HEADED=1` and, if needed, `JELLYSWARRM_CHROME_PATH`.
Keep the Docker image's Playwright version aligned with the Rust binding's driver.
