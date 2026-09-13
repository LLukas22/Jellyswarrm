use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker, Git LFS media, embedded Jellyfin Web, and Chrome"]
async fn two_chrome_clients_control_movies_from_both_backends() -> Result<()> {
    run_scenario("remote-control", Scenario::RemoteControl).await
}

pub(super) async fn run(
    fixture: &ServerFixture,
    auth: &Value,
    movies: &Value,
    controller_page: &Page,
    receiver_page: &Page,
) -> Result<()> {
    let token = required_string(auth, "/AccessToken")?;
    let target = wait_for_receiver(fixture, token).await?;
    for (index, title) in ["Night of the Living Dead", "Plan 9 from Outer Space"]
        .into_iter()
        .enumerate()
    {
        let id = item_id_named(movies, title)?;
        let server_id = required_string(auth, "/ServerId")?;
        controller_page
            .goto(
                &format!(
                    "{}/web/index.html#/details?id={id}&serverId={server_id}",
                    fixture.proxy_url
                ),
                None,
            )
            .await?;
        eprintln!("Casting {title} to receiver {target}");
        if index == 0 {
            controller_page
                .locator(
                    "button[aria-controls=app-remote-play-menu]:visible, .headerCastButton:visible",
                )
                .click(None)
                .await?;
            controller_page.locator(format!("#app-remote-play-menu [role=menuitem]:has-text(\"Chrome - Jellyfin Web\"), .actionSheetMenuItem[data-id={}]", serde_json::to_string(&target)?)).click(None).await?;
        }
        controller_page
            .locator(".btnPlay:visible")
            .first()
            .click(None)
            .await?;
        video_state(receiver_page, "!v.paused && v.currentTime > 1")
            .await
            .with_context(|| format!("receiver did not play {title}"))?;
        wait_for_session(fixture, token, |s| {
            s["Id"] == target && s["NowPlayingItem"]["Id"] == id
        })
        .await?;
        controller_page
            .locator(".nowPlayingBar .playPauseButton:visible")
            .click(None)
            .await?;
        video_state(receiver_page, "v.paused").await?;
        // Range input change is the same event consumed by Jellyfin's slider.
        controller_page.evaluate_expression("(() => { const s = document.querySelector('.nowPlayingBarPositionSlider'); s.value = '50'; s.dispatchEvent(new Event('change', {bubbles: true})); })()").await?;
        video_state(
            receiver_page,
            "Number.isFinite(v.duration) && Math.abs(v.currentTime - v.duration / 2) < 5",
        )
        .await?;
        controller_page
            .locator(".nowPlayingBar .playPauseButton:visible")
            .click(None)
            .await?;
        video_state(
            receiver_page,
            "!v.paused && v.currentTime > v.duration / 2 + 1",
        )
        .await?;
        controller_page
            .locator(".nowPlayingBar .stopButton:visible")
            .click(None)
            .await?;
        receiver_page.wait_for_function("() => [...document.querySelectorAll('video')].every(v => v.paused && (!v.currentSrc || v.currentTime === 0))", None).await?;
    }
    Ok(())
}
