//! Initialize console and rotating-file tracing; callers retain the worker guard until shutdown.
use crate::config::DATA_DIR;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
pub(super) fn initialize() -> tracing_appender::non_blocking::WorkerGuard {
    // Initialize file logging

    let file_appender = tracing_appender::rolling::daily(DATA_DIR.join("logs"), "jellyswarm.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Create an environment filter with configurable log level
    // Defaults to "jellyswarrm_proxy=info" but can be overridden with RUST_LOG env var
    // Examples:
    //   RUST_LOG=debug                           - Enable debug for all modules
    //   RUST_LOG=jellyswarrm_proxy=debug         - Enable debug for this app only
    //   RUST_LOG=jellyswarrm_proxy=trace,tower=info - Debug this app, info for tower
    let default_filter = "jellyswarrm_proxy=info,jellyfin_api=info";
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .init();

    _guard
}
