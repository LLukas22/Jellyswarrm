//! Opt-in browser integration test; see README.md beside this file.
use super::*;
#[path = "containers.rs"]
mod containers;
use playwright_rs::{
    protocol::{BrowserContext, Page, TracingStartOptions, TracingStopOptions},
    LaunchOptions, Playwright,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker, Git LFS media, embedded Jellyfin Web, and Chrome"]
async fn two_chrome_clients_control_movies_from_both_backends() -> Result<()> {
    let fixture = ServerFixture::start(false).await?;
    let auth = success_json(login(&fixture.client, &fixture.proxy_url, PASSWORD).await?).await?;
    let token = required_string(&auth, "/AccessToken")?;
    let user_id = required_string(&auth, "/User/Id")?;
    let views = wait_for_views(&fixture.client, &fixture.proxy_url, user_id, token).await?;
    let movies = wait_for_library_items(
        &fixture.client,
        &fixture.proxy_url,
        user_id,
        token,
        &views,
        ("Movies", "Movie", &expected_movie_names()),
    )
    .await?;

    let playwright = Playwright::launch().await?;
    let mut options =
        LaunchOptions::default().headless(std::env::var_os("JELLYSWARRM_BROWSER_HEADED").is_none());
    options = match std::env::var("JELLYSWARRM_CHROME_PATH") {
        Ok(path) => options.executable_path(path),
        Err(_) => options.channel("chrome".to_owned()),
    };
    // Explicit opt-in for diagnosing autoplay failures; normal runs use Chrome policy.
    if std::env::var_os("JELLYSWARRM_BROWSER_ALLOW_AUTOPLAY").is_some() {
        options = options.args(vec!["--autoplay-policy=no-user-gesture-required".to_owned()]);
    }
    let container_mode = std::env::var_os("JELLYSWARRM_BROWSER_CONTAINERS").is_some();
    if !container_mode {
        playwright_rs::install_browsers(Some(&["ffmpeg"])).await?;
    }
    let controller_container = if container_mode {
        Some(containers::ContainerBrowser::start().await?)
    } else {
        None
    };
    let controller_browser =
        containers::launch(&playwright, options.clone(), controller_container.as_ref()).await?;
    let result = async {
        let receiver_container = if container_mode { Some(containers::ContainerBrowser::start().await?) } else { None };
        let receiver_browser = containers::launch(&playwright, options, receiver_container.as_ref()).await?;
        let result = async {
            let video_dir = tempfile::tempdir()?;
            let options = playwright_rs::protocol::BrowserContextOptions::builder().record_video(playwright_rs::protocol::RecordVideo::new(
                    if container_mode { "/tmp/videos".to_owned() } else { video_dir.path().to_string_lossy().into_owned() }
                ).size(playwright_rs::protocol::Viewport { width: 1600, height: 1000 })).build();
            let controller = controller_browser.new_context_with_options(options.clone()).await?;
            let receiver = receiver_browser.new_context_with_options(options).await?;
            let artifacts = workspace_root().join("target/browser-test-artifacts");
            tokio::fs::create_dir_all(&artifacts).await?;
            for name in ["controller", "receiver"] {
                let _ = tokio::fs::remove_file(artifacts.join(format!("{name}.png"))).await;
                let _ = tokio::fs::remove_file(artifacts.join(format!("{name}.zip"))).await;
                let _ = tokio::fs::remove_file(artifacts.join(format!("{name}.webm"))).await;
            }
            let controller_page = prepare(&controller, "controller").await?;
            let receiver_page = prepare(&receiver, "receiver").await?;
            let mut result = tokio::time::timeout(Duration::from_secs(300), async {
                browser_login(&controller_page, &fixture.proxy_url).await?;
                browser_login(&receiver_page, &fixture.proxy_url).await?;
                let target = wait_for_receiver(&fixture, token).await?;
                for (index, title) in ["Night of the Living Dead", "Plan 9 from Outer Space"].into_iter().enumerate() {
                    let id = item_id_named(&movies, title)?;
                    let server_id = required_string(&auth, "/ServerId")?;
                    controller_page.goto(&format!("{}/web/index.html#/details?id={id}&serverId={server_id}", fixture.proxy_url), None).await?;
                    eprintln!("Casting {title} to receiver {target}");
                    if index == 0 {
                    controller_page.locator("button[aria-controls=app-remote-play-menu]:visible, .headerCastButton:visible").click(None).await?;
                    controller_page.locator(format!("#app-remote-play-menu [role=menuitem]:has-text(\"Chrome - Jellyfin Web\"), .actionSheetMenuItem[data-id={}]", serde_json::to_string(&target)?)).click(None).await?;
                    }
                    controller_page.locator(".btnPlay:visible").first().click(None).await?;
                    video_state(&receiver_page, "!v.paused && v.currentTime > 1").await
                        .with_context(|| format!("receiver did not play {title}"))?;
                    wait_for_session(&fixture, token, |s| s["Id"] == target && s["NowPlayingItem"]["Id"] == id).await?;
                    controller_page.locator(".nowPlayingBar .playPauseButton:visible").click(None).await?;
                    video_state(&receiver_page, "v.paused").await?;
                    // Range input change is the same event consumed by Jellyfin's slider.
                    controller_page.evaluate_expression("(() => { const s = document.querySelector('.nowPlayingBarPositionSlider'); s.value = '50'; s.dispatchEvent(new Event('change', {bubbles: true})); })()").await?;
                    video_state(&receiver_page, "Number.isFinite(v.duration) && Math.abs(v.currentTime - v.duration / 2) < 5").await?;
                    controller_page.locator(".nowPlayingBar .playPauseButton:visible").click(None).await?;
                    video_state(&receiver_page, "!v.paused && v.currentTime > v.duration / 2 + 1").await?;
                    controller_page.locator(".nowPlayingBar .stopButton:visible").click(None).await?;
                    receiver_page.wait_for_function("() => [...document.querySelectorAll('video')].every(v => v.paused && (!v.currentSrc || v.currentTime === 0))", None).await?;
                }
                Ok::<(), anyhow::Error>(())
            }).await.context("browser scenario exceeded five minutes").and_then(|result| result);
            for (name, context, page, container) in [("controller", &controller, &controller_page, controller_container.as_ref()), ("receiver", &receiver, &receiver_page, receiver_container.as_ref())] {
                if result.is_err() {
                    if let Err(error) = page.screenshot_to_file(&artifacts.join(format!("{name}.png")), None).await {
                        eprintln!("Could not save {name} screenshot: {error}");
                    }
                }
                if let Err(error) = containers::save_trace(context, container, artifacts.join(format!("{name}.zip"))).await {
                    eprintln!("Could not save {name} trace: {error}");
                    if result.is_ok() { result = Err(error.context(format!("export {name} trace"))); }
                }
                if let Err(error) = containers::save_video(context, page, container, artifacts.join(format!("{name}.webm"))).await {
                    eprintln!("Could not save {name} video: {error}");
                    if result.is_ok() { result = Err(error.context(format!("export {name} video"))); }
                }
            }
            result
        }.await;
        let closed = receiver_browser.close().await;
        result?;
        closed?;
        Ok::<(), anyhow::Error>(())
    }.await;
    let closed = controller_browser.close().await;
    result?;
    closed?;
    Ok(())
}

async fn prepare(context: &BrowserContext, role: &str) -> Result<Page> {
    context.add_init_script(&format!("try {{ localStorage.setItem('_deviceId2', '{role}-jellyswarrm-browser'); }} catch (_) {{}}" )).await?;
    context
        .tracing()
        .await?
        .start(Some(
            TracingStartOptions::default()
                .screenshots(true)
                .snapshots(true),
        ))
        .await?;
    let page = context.new_page().await?;
    page.set_viewport_size(playwright_rs::protocol::Viewport {
        width: 1600,
        height: 1000,
    })
    .await?;
    page.set_default_timeout(30_000.0).await;
    Ok(page)
}

async fn browser_login(page: &Page, base: &str) -> Result<()> {
    page.goto(&format!("{base}/web/index.html#/login"), None)
        .await?;
    page.wait_for_function("() => ['#txtManualName', '.btnManual'].some(s => document.querySelector(s)?.getClientRects().length)", None).await?;
    if !page.locator("#txtManualName").is_visible().await? {
        page.locator(".btnManual:visible").click(None).await?;
    }
    page.locator("#txtManualName").fill(USERNAME, None).await?;
    page.locator("#txtManualPassword")
        .fill(PASSWORD, None)
        .await?;
    page.locator(".manualLoginForm button[type=submit]")
        .click(None)
        .await?;
    page.wait_for_function(
        "() => !location.hash.includes('login') && !location.hash.includes('selectserver')",
        None,
    )
    .await?;
    Ok(())
}

async fn wait_for_receiver(fixture: &ServerFixture, token: &str) -> Result<String> {
    let session = wait_for_session(fixture, token, |s| {
        s["DeviceId"] == "receiver-jellyswarrm-browser" && s["SupportsRemoteControl"] == true
    })
    .await?;
    Ok(required_string(&session, "/Id")?.to_owned())
}

async fn wait_for_session(
    fixture: &ServerFixture,
    token: &str,
    matches: impl Fn(&Value) -> bool,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let sessions = success_json(
            authenticated(
                fixture
                    .client
                    .get(format!("{}/Sessions", fixture.proxy_url)),
                token,
            )
            .send()
            .await?,
        )
        .await?;
        if let Some(session) = sessions
            .as_array()
            .context("Sessions must be an array")?
            .iter()
            .find(|s| matches(s))
        {
            return Ok(session.clone());
        }
        if Instant::now() >= deadline {
            bail!("expected receiver session state was not observed: {sessions}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn video_state(page: &Page, predicate: &str) -> Result<()> {
    page.wait_for_function(
        &format!("() => [...document.querySelectorAll('video')].some(v => {predicate})"),
        None,
    )
    .await?;
    Ok(())
}
