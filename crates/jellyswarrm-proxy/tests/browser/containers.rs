use super::*;
use playwright_rs::protocol::Browser;
use testcontainers::{core::WaitFor, runners::AsyncRunner, ContainerAsync, GenericImage, ImageExt};

/// Owns the container until the browser connection has been closed.
pub(super) struct ContainerBrowser {
    endpoint: String,
    _container: ContainerAsync<GenericImage>,
}

impl ContainerBrowser {
    pub async fn start() -> Result<Self> {
        anyhow::ensure!(
            cfg!(target_os = "linux"),
            "container browsers require Linux host networking"
        );
        let port = available_port()?;
        let path = uuid::Uuid::new_v4().simple().to_string();
        let mut request = GenericImage::new("jellyswarrm-browser-tests", "1.63.0")
            .with_wait_for(WaitFor::message_on_stdout("Jellyswarrm browser ready"))
            .with_network("host")
            .with_shm_size(1024 * 1024 * 1024)
            .with_env_var("PLAYWRIGHT_PORT", port.to_string())
            .with_env_var("PLAYWRIGHT_WS_PATH", &path);
        if std::env::var_os("JELLYSWARRM_BROWSER_ALLOW_AUTOPLAY").is_some() {
            request = request.with_env_var("JELLYSWARRM_BROWSER_ALLOW_AUTOPLAY", "1");
        }
        let container = request
            .start()
            .await
            .context("start browser container (build with just browser-image)")?;
        eprintln!("Started Chrome container {}", container.id());
        Ok(Self {
            endpoint: format!("ws://127.0.0.1:{port}/{path}"),
            _container: container,
        })
    }

    pub async fn connect(&self, playwright: &Playwright) -> Result<Browser> {
        Ok(playwright.chromium().connect(&self.endpoint, None).await?)
    }
}

pub(super) async fn save_trace(
    context: &BrowserContext,
    container: Option<&ContainerBrowser>,
    destination: PathBuf,
) -> Result<()> {
    // playwright-rs Artifact::save_as writes on the server side. Explicitly
    // copy remote archives out before Testcontainers removes the browser.
    let path = if container.is_some() {
        "/tmp/trace.zip".to_owned()
    } else {
        destination.to_string_lossy().into_owned()
    };
    context
        .tracing()
        .await?
        .stop(Some(TracingStopOptions::default().path(&path)))
        .await?;
    export_file(container, path, destination).await
}

pub(super) async fn save_video(
    context: &BrowserContext,
    page: &Page,
    container: Option<&ContainerBrowser>,
    destination: PathBuf,
) -> Result<()> {
    let video = page.video().context("page has no video recording")?;
    // Closing the context flushes the recording; keep the browser/container
    // alive until both the save and the remote copy have completed.
    context.close().await?;
    let path = if container.is_some() {
        "/tmp/recording.webm".to_owned()
    } else {
        destination.to_string_lossy().into_owned()
    };
    video.save_as(&path).await?;
    export_file(container, path, destination).await
}

async fn export_file(
    container: Option<&ContainerBrowser>,
    path: String,
    destination: PathBuf,
) -> Result<()> {
    if let Some(container) = container {
        container
            ._container
            .copy_file_from(path, destination.clone())
            .await?;
    }
    anyhow::ensure!(
        tokio::fs::metadata(&destination).await?.len() > 0,
        "empty browser artifact: {}",
        destination.display()
    );
    Ok(())
}

pub(super) async fn launch(
    playwright: &Playwright,
    options: LaunchOptions,
    container: Option<&ContainerBrowser>,
) -> Result<Browser> {
    match container {
        Some(container) => container.connect(playwright).await,
        None => Ok(playwright.chromium().launch_with_options(options).await?),
    }
}
