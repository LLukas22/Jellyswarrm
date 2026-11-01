//! Jellyswarrm server lifecycle. The executable supplies the Tokio runtime.
mod application;
mod auth;
mod client_sessions;
mod config;
mod domain;
mod http;
#[cfg(test)]
mod http_tests;
mod libraries;
mod media;
mod persistence;
mod playback;
mod protocol;
mod servers;
mod startup;
mod syncplay;
mod translation;
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
    startup::run().await
}
