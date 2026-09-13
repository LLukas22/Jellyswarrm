use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker, Git LFS media, embedded Jellyfin Web, and Chrome"]
async fn two_chrome_clients_switch_media_and_rejoin_syncplay() -> Result<()> {
    run_scenario("syncplay", Scenario::SyncPlay).await
}

pub(super) async fn run(
    fixture: &ServerFixture,
    auth: &Value,
    movies: &Value,
    leader: &Page,
    member: &Page,
) -> Result<()> {
    let token = required_string(auth, "/AccessToken")?;
    for role in ["controller", "receiver"] {
        wait_for_session(fixture, token, |s| {
            s["DeviceId"] == format!("{role}-jellyswarrm-browser")
                && s["SupportsRemoteControl"] == true
        })
        .await?;
    }
    eprintln!("Creating SyncPlay group through leader UI");
    open_menu(leader).await?;
    leader
        .locator("#app-sync-play-menu [role=menuitem]:has([data-testid=GroupAddIcon])")
        .click(None)
        .await?;
    wait_for_groups(fixture, token, 1).await?;
    eprintln!("Joining SyncPlay group through member UI");
    join_group(member).await?;
    play_movie(fixture, auth, movies, leader, "Night of the Living Dead").await?;
    assert_playing(
        fixture,
        token,
        movies,
        leader,
        "controller",
        "Night of the Living Dead",
    )
    .await?;
    assert_playing(
        fixture,
        token,
        movies,
        member,
        "receiver",
        "Night of the Living Dead",
    )
    .await?;
    eprintln!("Checking group pause, seek and resume");
    toggle_playback(leader).await?;
    for page in [leader, member] {
        video_state(page, "v.paused && v.currentTime > 0").await?;
    }
    leader.evaluate_expression("(() => { const s = document.querySelector('.osdPositionSlider'); s.value = '50'; s.dispatchEvent(new Event('change', { bubbles: true })); })()").await?;
    for page in [leader, member] {
        video_state(page, "v.paused && Number.isFinite(v.duration) && Math.abs(v.currentTime - v.duration / 2) < 3").await.context("group seek did not reach both players")?;
    }
    let leader_position: f64 = leader
        .evaluate_value("document.querySelector('video').currentTime")
        .await?
        .parse()?;
    let member_position: f64 = member
        .evaluate_value("document.querySelector('video').currentTime")
        .await?
        .parse()?;
    anyhow::ensure!(
        (leader_position - member_position).abs() < 2.0,
        "group players differ by more than two seconds after seek"
    );
    toggle_playback(leader).await?;
    for page in [leader, member] {
        video_state(page, "!v.paused && v.currentTime > v.duration / 2 + 1").await?;
    }

    eprintln!("Switching the group to media on another backend");
    play_movie(fixture, auth, movies, leader, "Plan 9 from Outer Space").await?;
    for (page, role) in [(leader, "controller"), (member, "receiver")] {
        assert_playing(
            fixture,
            token,
            movies,
            page,
            role,
            "Plan 9 from Outer Space",
        )
        .await?;
    }

    eprintln!("Leaving and rejoining during playback");
    leave_group(member).await?;
    wait_for_groups(fixture, token, 1).await?;
    video_state(leader, "!v.paused").await?;
    open_menu(member).await?;
    member
        .locator("#app-sync-play-menu button:has([data-testid=PersonAddIcon])")
        .wait_for(None)
        .await?;
    member.locator("body").press("Escape", None).await?;
    video_state(leader, "!v.paused").await?;
    let departed_source = member
        .evaluate_value("document.querySelector('video').currentSrc")
        .await?;
    play_movie(fixture, auth, movies, leader, "Night of the Living Dead").await?;
    assert_playing(
        fixture,
        token,
        movies,
        leader,
        "controller",
        "Night of the Living Dead",
    )
    .await?;
    // A departed client keeps playing its own media through the group's switch.
    video_state(member, "!v.paused").await?;
    anyhow::ensure!(
        member
            .evaluate_value("document.querySelector('video').currentSrc")
            .await?
            == departed_source,
        "departed player followed the group's media switch"
    );
    join_group(member).await?;
    assert_playing(
        fixture,
        token,
        movies,
        member,
        "receiver",
        "Night of the Living Dead",
    )
    .await?;
    toggle_playback(leader).await?;
    for page in [leader, member] {
        video_state(page, "v.paused && v.currentTime > 0").await?;
    }
    let leader_position: f64 = leader
        .evaluate_value("document.querySelector('video').currentTime")
        .await?
        .parse()?;
    let member_position: f64 = member
        .evaluate_value("document.querySelector('video').currentTime")
        .await?
        .parse()?;
    anyhow::ensure!(
        (leader_position - member_position).abs() < 2.0,
        "rejoined player did not synchronize its position"
    );
    toggle_playback(leader).await?;
    for page in [leader, member] {
        video_state(page, "!v.paused").await?;
    }

    eprintln!("Leaving SyncPlay from both clients");
    leave_group(member).await?;
    wait_for_groups(fixture, token, 1).await?;
    video_state(leader, "!v.paused").await?;
    leave_group(leader).await?;
    wait_for_groups(fixture, token, 0).await?;
    Ok(())
}

async fn play_movie(
    fixture: &ServerFixture,
    auth: &Value,
    movies: &Value,
    page: &Page,
    title: &str,
) -> Result<()> {
    let id = item_id_named(movies, title)?;
    let server = required_string(auth, "/ServerId")?;
    page.goto(
        &format!(
            "{}/web/index.html#/details?id={id}&serverId={server}",
            fixture.proxy_url
        ),
        None,
    )
    .await?;
    page.locator(".btnPlay:visible").first().click(None).await?;
    Ok(())
}

async fn assert_playing(
    fixture: &ServerFixture,
    token: &str,
    movies: &Value,
    page: &Page,
    role: &str,
    title: &str,
) -> Result<()> {
    let id = item_id_named(movies, title)?;
    wait_for_session(fixture, token, |s| {
        s["DeviceId"] == format!("{role}-jellyswarrm-browser") && s["NowPlayingItem"]["Id"] == id
    })
    .await?;
    video_state(page, "!v.paused && v.currentTime > 1")
        .await
        .with_context(|| format!("{role} did not play {title}"))
}

async fn join_group(page: &Page) -> Result<()> {
    open_menu(page).await?;
    page.locator("#app-sync-play-menu button:has([data-testid=PersonAddIcon])")
        .click(None)
        .await?;
    wait_for_menu_closed(page).await?;
    // Confirm this browser received the group update, even when users share a name.
    open_menu(page).await?;
    page.locator("#app-sync-play-menu button:has([data-testid=PersonRemoveIcon])")
        .wait_for(None)
        .await?;
    page.locator("body").press("Escape", None).await?;
    Ok(())
}

async fn wait_for_menu_closed(page: &Page) -> Result<()> {
    page.wait_for_function("() => { const menu = document.querySelector('#app-sync-play-menu'); return !menu || getComputedStyle(menu).visibility === 'hidden'; }", None).await?;
    Ok(())
}

async fn open_menu(page: &Page) -> Result<()> {
    if page.locator("video").is_visible().await? {
        show_controls(page).await?;
    }
    page.locator("button[aria-controls=app-sync-play-menu]:visible")
        .click(None)
        .await?;
    Ok(())
}

async fn toggle_playback(page: &Page) -> Result<()> {
    show_controls(page).await?;
    page.locator(".btnPause:visible").click(None).await?;
    Ok(())
}

async fn leave_group(page: &Page) -> Result<()> {
    open_menu(page).await?;
    page.locator("#app-sync-play-menu button:has([data-testid=PersonRemoveIcon])")
        .click(None)
        .await?;
    wait_for_menu_closed(page).await
}

async fn wait_for_groups(fixture: &ServerFixture, token: &str, count: usize) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let groups = success_json(
            authenticated(
                fixture
                    .client
                    .get(format!("{}/SyncPlay/List", fixture.proxy_url)),
                token,
            )
            .send()
            .await?,
        )
        .await?;
        if groups
            .as_array()
            .context("group list must be an array")?
            .len()
            == count
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("expected {count} SyncPlay groups; got {groups}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn show_controls(page: &Page) -> Result<()> {
    // The OSD overlays the video element, so move the real pointer instead of
    // asking Playwright to hover an element that cannot receive pointer events.
    page.mouse().move_to(700.0, 400.0, None).await?;
    page.mouse().move_to(710.0, 410.0, None).await?;
    Ok(())
}
