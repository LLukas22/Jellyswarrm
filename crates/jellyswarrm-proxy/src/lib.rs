//! Jellyswarrm server lifecycle. The executable supplies the Tokio runtime.
mod app;
mod application;
mod auth;
mod config;
mod domain;
mod http;
#[cfg(test)]
mod http_tests;
mod libraries;
mod media;
mod models;
mod outbound;
mod persistence;
mod playback;
mod processors;
mod protocol;
mod servers;
mod sessions;
mod syncplay;
mod ui;
mod upstream;

#[cfg(test)]
mod test_support;

/// Run the Jellyswarrm server until shutdown.
///
/// Requires a Tokio runtime. Initializes process-wide logging, loads configuration,
/// connects to the database, and starts the HTTP listener and background services.
/// Call once per process.
///
/// ```no_run
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     jellyswarrm_proxy::run().await
/// }
/// ```
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    app::run().await
}
