# Remote-control browser test

This verifies that one Jellyfin Web client can control another through the proxy
across two media backends: play, pause, seek, resume, and stop.

The Rust Playwright test reuses the server integration fixture. Each Chrome
instance runs in its own container; Testcontainers manages startup and cleanup.

Run on Linux x86-64 with Docker, Git LFS, and the usual Rust/UI build prerequisites:

```sh
just browser-integration-test-docker
```

CI runs the same command. Videos (`controller.webm`, `receiver.webm`), traces,
and failure screenshots go to `target/browser-test-artifacts/` and are uploaded
as `browser-test-artifacts` for seven days, including on test failure.

For native Chrome debugging, use `just browser-integration-test` with
`JELLYSWARRM_BROWSER_HEADED=1` and, if needed, `JELLYSWARRM_CHROME_PATH`.
Keep the Docker image's Playwright version aligned with the Rust binding's driver.
