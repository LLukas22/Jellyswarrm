//! Supply the runtime for the Jellyswarrm library's process entry point.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    jellyswarrm_proxy::run().await
}
