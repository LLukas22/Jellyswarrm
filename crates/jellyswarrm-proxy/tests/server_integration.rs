use std::{
    collections::HashSet,
    net::TcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use reqwest::{Client, Response, StatusCode, Url};
use serde_json::{json, Value};
use tempfile::TempDir;
use testcontainers::compose::DockerCompose;
use tokio::process::{Child, Command};

const USERNAME: &str = "test";
const PASSWORD: &str = "test";
const AUTHORIZATION: &str = "MediaBrowser Client=\"Jellyswarrm Integration Tests\", Device=\"Test Runner\", DeviceId=\"jellyswarrm-integration-tests\", Version=\"1.0.0\"";
const SEERR_AUTHORIZATION: &str =
    "MediaBrowser Client=\"Seerr\", Device=\"Seerr\", DeviceId=\"BOT_seerr\", Version=\"3.4.0\"";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(3 * 60);
// Show merging converges only once both tv servers finished scanning, which
// can lag behind the movie/music catalogs on a cold stack.
const SHOW_TIMEOUT: Duration = Duration::from_secs(10 * 60);

struct ServerProcess {
    child: Child,
    _data_dir: TempDir,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

struct ServerFixture {
    client: Client,
    proxy_url: String,
    // Drop the proxy before the upstream stack; ServerProcess owns its temp state.
    _proxy: ServerProcess,
    _compose: DockerCompose,
}

impl ServerFixture {
    async fn start(deduplicate_media: bool) -> Result<Self> {
        let workspace = workspace_root();
        ensure_media_fixture_is_present(&workspace)?;

        let compose_files = vec![
            workspace.join("dev/docker-compose.yml"),
            workspace.join("dev/docker-compose.integration.yml"),
        ];
        let mut compose = DockerCompose::with_local_client(compose_files).with_wait(false);
        tokio::time::timeout(STARTUP_TIMEOUT, compose.up())
            .await
            .context("timed out starting the Jellyfin development stack")??;

        let upstreams = upstream_urls(&compose).await?;
        let data_dir = tempfile::tempdir().context("failed to create Jellyswarrm test data dir")?;
        let proxy_port = available_port()?;
        write_proxy_config(data_dir.path(), proxy_port, &upstreams, deduplicate_media)?;
        let mut proxy = start_proxy(data_dir, proxy_port)?;
        let proxy_url = format!("http://127.0.0.1:{proxy_port}");
        let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
        wait_for_proxy(&client, &proxy_url, &mut proxy.child).await?;

        Ok(Self {
            client,
            proxy_url,
            _proxy: proxy,
            _compose: compose,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker and the Git LFS media fixtures"]
async fn user_can_login_browse_merged_libraries_and_stream_mapped_media() -> Result<()> {
    let fixture = ServerFixture::start(false).await?;
    let ServerFixture {
        client, proxy_url, ..
    } = &fixture;

    let bad_login = login(client, proxy_url, "wrong-password").await?;
    assert_eq!(bad_login.status(), StatusCode::UNAUTHORIZED);

    let login = success_json(login(client, proxy_url, PASSWORD).await?).await?;
    let token = required_string(&login, "/AccessToken")?;
    let user_id = required_string(&login, "/User/Id")?;
    assert_eq!(required_string(&login, "/User/Name")?, USERNAME);

    let views = wait_for_views(client, proxy_url, user_id, token).await?;
    let movies = wait_for_library_items(
        client,
        proxy_url,
        user_id,
        token,
        &views,
        ("Movies", "Movie", &expected_movie_names()),
    )
    .await?;
    let view_names = item_names(&views)?;
    assert!(view_names.contains("Movies"));
    assert!(view_names.contains("Shows"));
    assert!(view_names.contains("Music"));

    let movie_names = item_names(&movies)?;
    let expected = HashSet::from(expected_movie_names());
    assert!(
        expected.is_subset(&movie_names),
        "movie catalog: {movie_names:?}"
    );
    assert_eq!(
        items(&movies)?.len(),
        5,
        "the shared movie should expose one labeled source per server"
    );

    for movie_name in ["Night of the Living Dead", "Plan 9 from Outer Space"] {
        let item_id = item_id_named(&movies, movie_name)?;
        verify_playback(client, proxy_url, user_id, token, item_id)
            .await
            .with_context(|| format!("failed playback check for {movie_name}"))?;
    }

    let music = wait_for_library_items(
        client,
        proxy_url,
        user_id,
        token,
        &views,
        ("Music", "Audio", &["01 - Aria", "01 - Death Valley Waltz"]),
    )
    .await?;
    verify_audio_playback(
        client,
        proxy_url,
        user_id,
        token,
        item_named(&music, "01 - Death Valley Waltz")?,
    )
    .await
    .context("failed playback check for Death Valley Waltz")?;
    verify_seerr_integration(client, proxy_url).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker and the Git LFS media fixtures"]
async fn saved_playlists_support_lifecycle_sharing_and_reject_mixed_servers() -> Result<()> {
    run_saved_playlist_test(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker and Git LFS; exposes duplicate-entry IDs in Jellyfin 12"]
async fn saved_playlist_duplicate_songs_have_independently_addressable_entries() -> Result<()> {
    run_saved_playlist_test(true).await
}

async fn run_saved_playlist_test(duplicate_song: bool) -> Result<()> {
    let fixture = ServerFixture::start(false).await?;
    let client = &fixture.client;
    let base_url = &fixture.proxy_url;
    let login = success_json(login(client, base_url, PASSWORD).await?).await?;
    let token = required_string(&login, "/AccessToken")?;
    let user_id = required_string(&login, "/User/Id")?;
    let views = wait_for_views(client, base_url, user_id, token).await?;
    let music = wait_for_library_items(
        client,
        base_url,
        user_id,
        token,
        &views,
        ("Music", "Audio", &["01 - Aria", "01 - Death Valley Waltz"]),
    )
    .await?;
    let upstreams = upstream_urls(&fixture._compose).await?;
    let music_upstream = &upstreams
        .iter()
        .find(|(name, _, _)| *name == "Music 1")
        .context("missing Music 1")?
        .1;
    verify_saved_playlist(
        client,
        base_url,
        user_id,
        token,
        &music,
        music_upstream,
        duplicate_song,
    )
    .await
    .context("saved playlist lifecycle failed")
}

// Show merging with Jellyfin v12 multi-versions: with deduplicate_media
// enabled, "One Step Beyond" exists on both tv servers (same Tvdb ids via the
// .nfo fixtures) and must collapse into a single series whose shared season
// and episode merge, while the single-server "The Cisco Kid" passes through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker and the Git LFS media fixtures"]
async fn merged_shows_preserve_season_navigation_versions_and_latest_identity() -> Result<()> {
    let fixture = ServerFixture::start(true).await?;
    let ServerFixture {
        client, proxy_url, ..
    } = &fixture;

    let login = success_json(login(client, proxy_url, PASSWORD).await?).await?;
    let token = required_string(&login, "/AccessToken")?;
    let user_id = required_string(&login, "/User/Id")?;

    let views = wait_for_views(client, proxy_url, user_id, token).await?;
    wait_for_library_items(
        client,
        proxy_url,
        user_id,
        token,
        &views,
        ("Shows", "Series", &["One Step Beyond", "The Cisco Kid"]),
    )
    .await?;
    // Converge on the collapsed aggregate: early in a library scan only one
    // server's copy may be visible (an unlabeled singleton), so the series id
    // is re-resolved on every poll until the aggregate serves all seasons.
    // Season 1 exists on both servers and merges; seasons 2 and 3 are unique
    // to one server each.
    let (seasons, _) = wait_for_merged_seasons(client, proxy_url, user_id, token, &views).await?;
    assert_eq!(
        item_names(&seasons)?,
        HashSet::from(["Season 1", "Season 2", "Season 3"]),
        "shared season must collapse instead of being labeled per server"
    );

    let shows = wait_for_library_items(
        client,
        proxy_url,
        user_id,
        token,
        &views,
        ("Shows", "Series", &["One Step Beyond", "The Cisco Kid"]),
    )
    .await?;
    assert_eq!(
        item_names(&shows)?,
        HashSet::from(["One Step Beyond", "The Cisco Kid"]),
        "shared show must collapse instead of being labeled per server"
    );

    // S01E02 exists on both servers and merges into one episode advertising
    // one media source per server; the other two episodes stay single.
    let (episode, series_id) =
        wait_for_merged_episode(client, proxy_url, user_id, token, &views).await?;
    let episode_id = required_string(&episode, "/Id")?;
    assert_eq!(
        episode["MediaSourceCount"].as_i64(),
        Some(2),
        "merged episode must advertise one version per server"
    );

    let show_seasons = success_json(
        authenticated(
            client
                .get(format!("{proxy_url}/Shows/{series_id}/Seasons"))
                .query(&[
                    ("userId", user_id),
                    (
                        "Fields",
                        "ItemCounts,PrimaryImageAspectRatio,CanDelete,MediaSourceCount",
                    ),
                ]),
            token,
        )
        .send()
        .await?,
    )
    .await?;
    assert_eq!(show_seasons["TotalRecordCount"], 3);
    let season_items = items(&show_seasons)?;
    assert_eq!(
        season_items
            .iter()
            .map(|season| season["IndexNumber"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    for (season, episode_number) in season_items.iter().zip([2, 21, 34]) {
        let season_id = required_string(season, "/Id")?;
        // Use the response's parent link, just as the season details UI does.
        let parent_series_id = required_string(season, "/SeriesId")?;
        assert_eq!(
            parent_series_id, series_id,
            "seasons must retain the merged series navigation link"
        );
        let episodes = success_json(
            authenticated(
                client
                    .get(format!("{proxy_url}/Shows/{parent_series_id}/Episodes"))
                    .query(&[
                        ("userId", user_id),
                        ("seasonId", season_id),
                        (
                            "Fields",
                            "ItemCounts,PrimaryImageAspectRatio,CanDelete,MediaSourceCount,Overview",
                        ),
                    ]),
                token,
            )
            .send()
            .await?,
        )
        .await?;
        assert_eq!(
            episodes["TotalRecordCount"], 1,
            "season filter must not leak episodes from another season"
        );
        let filtered = items(&episodes)?;
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["ParentIndexNumber"], season["IndexNumber"]);
        assert_eq!(filtered[0]["IndexNumber"], episode_number);
        if episode_number == 2 {
            assert_eq!(required_string(&filtered[0], "/Id")?, episode_id);
            assert_eq!(filtered[0]["MediaSourceCount"], 2);
            verify_version_switching(client, proxy_url, user_id, token, episode_id).await?;
        } else {
            verify_playback(
                client,
                proxy_url,
                user_id,
                token,
                required_string(&filtered[0], "/Id")?,
            )
            .await?;
        }
    }

    // The merged detail response exposes both backend versions for playback
    // source selection.
    let detail = success_json(
        authenticated(
            client
                .get(format!("{proxy_url}/Users/{user_id}/Items/{episode_id}"))
                .query(&[("Fields", "MediaSources")]),
            token,
        )
        .send()
        .await?,
    )
    .await?;
    let sources = detail["MediaSources"]
        .as_array()
        .context("detail response did not contain MediaSources")?;
    assert_eq!(
        sources.len(),
        2,
        "merged episode detail must list both versions"
    );

    verify_playback(client, proxy_url, user_id, token, episode_id)
        .await
        .context("failed playback check for merged episode")?;

    verify_latest_aggregate(
        client,
        proxy_url,
        user_id,
        token,
        &views,
        ("Shows", "Series", "One Step Beyond"),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker and the Git LFS media fixtures"]
async fn merged_movies_preserve_versions_and_latest_identity() -> Result<()> {
    let fixture = ServerFixture::start(true).await?;
    let ServerFixture {
        client, proxy_url, ..
    } = &fixture;
    let login = success_json(login(client, proxy_url, PASSWORD).await?).await?;
    let token = required_string(&login, "/AccessToken")?;
    let user_id = required_string(&login, "/User/Id")?;
    let views = wait_for_views(client, proxy_url, user_id, token).await?;

    // Unique titles from each movie server ensure both catalogs have scanned
    // before checking that the shared movie collapses in the latest feed.
    let movies = wait_for_library_items(
        client,
        proxy_url,
        user_id,
        token,
        &views,
        (
            "Movies",
            "Movie",
            &[
                "Big Buck Bunny",
                "Night of the Living Dead",
                "Plan 9 from Outer Space",
                "Sintel",
            ],
        ),
    )
    .await?;

    let aggregate = item_named(&movies, "Big Buck Bunny")?;
    assert_eq!(aggregate["MediaSourceCount"], 2);
    verify_version_switching(
        client,
        proxy_url,
        user_id,
        token,
        required_string(aggregate, "/Id")?,
    )
    .await?;
    verify_latest_aggregate(
        client,
        proxy_url,
        user_id,
        token,
        &views,
        ("Movies", "Movie", "Big Buck Bunny"),
    )
    .await?;

    Ok(())
}

async fn verify_latest_aggregate(
    client: &Client,
    proxy_url: &str,
    user_id: &str,
    token: &str,
    views: &Value,
    catalog: (&str, &str, &str),
) -> Result<()> {
    let (view_name, item_type, shared_name) = catalog;
    let view_id = item_id_named(views, view_name)?;
    let before = fetch_items(
        client,
        proxy_url,
        user_id,
        token,
        view_id,
        item_type,
        "ProviderIds,MediaSources,DateCreated,Path",
    )
    .await?;
    let aggregate = item_named(&before, shared_name)?;
    assert!(
        aggregate["ProviderIds"]
            .as_object()
            .is_some_and(|ids| !ids.is_empty()),
        "{shared_name} must have provider IDs before loading Latest: {aggregate}"
    );

    // Preserve the SDK's repeated lowercase fields parameters on the wire.
    // Rewriting each occurrence to the same comma list makes Jellyfin omit
    // ProviderIds, preventing the two backend copies from deduplicating.
    let latest = success_json(
        authenticated(
            client.get(format!("{proxy_url}/Items/Latest")).query(&[
                ("userId", user_id),
                ("parentId", view_id),
                ("fields", "PrimaryImageAspectRatio"),
                ("fields", "Path"),
            ]),
            token,
        )
        .send()
        .await?,
    )
    .await?;
    let latest_items = latest
        .as_array()
        .context("Latest response must be a bare array")?;
    let shared: Vec<_> = latest_items
        .iter()
        .filter(|item| {
            item["Name"]
                .as_str()
                .is_some_and(|name| name.starts_with(shared_name))
        })
        .collect();
    assert_eq!(
        shared.len(),
        1,
        "{view_name} Latest must contain one shared aggregate: {latest}"
    );
    let latest_aggregate = shared[0];
    assert_eq!(latest_aggregate["Name"], shared_name, "no server suffix");
    assert_eq!(latest_aggregate["Type"], item_type);
    assert_eq!(latest_aggregate["Id"], aggregate["Id"]);
    assert_eq!(latest_aggregate["ProviderIds"], aggregate["ProviderIds"]);
    assert!(!required_string(latest_aggregate, "/Path")?.is_empty());
    chrono::DateTime::parse_from_rfc3339(required_string(latest_aggregate, "/DateCreated")?)
        .context("Latest must retain a valid DateCreated for sorting")?;

    let after = fetch_items(
        client,
        proxy_url,
        user_id,
        token,
        view_id,
        item_type,
        "ProviderIds",
    )
    .await?;
    assert_eq!(item_ids_named(&after, shared_name)?.len(), 1);
    let after_aggregate = item_named(&after, shared_name)?;
    assert_eq!(after_aggregate["Id"], aggregate["Id"]);
    assert_eq!(after_aggregate["ProviderIds"], aggregate["ProviderIds"]);
    Ok(())
}

// Regression test for the WebOS client (#173): the official webOS app loads the UI
// from `<server>/web/index.html` (see jellyfin-webos `frontend/js/index.js`) and
// fetches every asset relative to that page. All `/web/*` requests must be served
// from the embedded dist, not forwarded to an upstream server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker and the Git LFS media fixtures"]
async fn web_ui_is_served_under_web_prefix_for_webos_clients() -> Result<()> {
    let fixture = ServerFixture::start(false).await?;
    let ServerFixture {
        client, proxy_url, ..
    } = &fixture;

    let index = client
        .get(format!("{proxy_url}/web/index.html"))
        .send()
        .await?
        .error_for_status()
        .context("/web/index.html should be served from the embedded dist")?;
    let index_body = index.text().await?;
    assert!(
        index_body.contains("main.jellyfin.bundle.js"),
        "/web/index.html should return the embedded jellyfin-web entry point"
    );

    let manifest = client
        .get(format!("{proxy_url}/web/manifest.json"))
        .send()
        .await?
        .error_for_status()
        .context("/web/manifest.json should be served from the embedded dist")?;
    let manifest_body: Value = manifest.json().await?;
    assert_eq!(manifest_body["name"], json!("Jellyfin"));

    let bundle = client
        .get(format!("{proxy_url}/web/main.jellyfin.bundle.js"))
        .send()
        .await?
        .error_for_status()
        .context("/web/main.jellyfin.bundle.js should be served from the embedded dist")?;
    let content_type = bundle
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/javascript")
            || content_type.starts_with("application/javascript"),
        "JS bundle must have a JavaScript MIME type, got: {content_type}"
    );

    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("proxy crate must be inside the workspace")
        .to_path_buf()
}

fn ensure_media_fixture_is_present(workspace: &Path) -> Result<()> {
    for path in [
        "dev/media/movies/server-1/Big Buck Bunny (2008)/Big Buck Bunny (2008).mp4",
        "dev/media/music/server-2/Lucas Gonze/Ghost Solos (2010)/01 - Death Valley Waltz.ogg",
    ] {
        let fixture = workspace.join(path);
        let size = fixture
            .metadata()
            .with_context(|| {
                format!(
                    "missing media fixture {}; run `just media`",
                    fixture.display()
                )
            })?
            .len();
        if size < 1024 {
            bail!(
                "media fixture {} is an LFS pointer; run `just media`",
                fixture.display()
            );
        }
    }
    Ok(())
}

async fn upstream_urls(compose: &DockerCompose) -> Result<Vec<(&'static str, String, i32)>> {
    let services = [
        ("jellyfin-movies", "Movies 1", 101),
        ("jellyfin-tvshows", "Shows 1", 101),
        ("jellyfin-music", "Music 1", 101),
        ("jellyfin-movies-2", "Movies 2", 100),
        ("jellyfin-tvshows-2", "Shows 2", 100),
        ("jellyfin-music-2", "Music 2", 100),
    ];
    let mut urls = Vec::with_capacity(services.len());
    for (service_name, display_name, priority) in services {
        let container = compose
            .service(service_name)
            .with_context(|| format!("Compose service {service_name} was not discovered"))?;
        let host = container.get_host().await?;
        let port = container.get_host_port_ipv4(8096).await?;
        urls.push((display_name, format!("http://{host}:{port}"), priority));
    }
    Ok(urls)
}

fn available_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn write_proxy_config(
    data_dir: &Path,
    proxy_port: u16,
    upstreams: &[(&str, String, i32)],
    deduplicate_media: bool,
) -> Result<()> {
    let mut config = format!(
        "host = \"127.0.0.1\"\nport = {proxy_port}\ninclude_server_name_in_media = false\nmerge_libraries = true\ndeduplicate_media = {deduplicate_media}\nserver_background_check_interval_secs = 1\n"
    );
    for (name, url, priority) in upstreams {
        config.push_str(&format!(
            "\n[[preconfigured_servers]]\nurl = \"{url}\"\nname = \"{name}\"\npriority = {priority}\nmedia_streaming_mode = \"Proxy\"\n"
        ));
    }
    std::fs::write(data_dir.join("jellyswarrm.toml"), config)
        .context("failed to write Jellyswarrm integration config")
}

fn start_proxy(data_dir: TempDir, proxy_port: u16) -> Result<ServerProcess> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jellyswarrm-proxy"));
    command
        .env("JELLYSWARRM_DATA_DIR", data_dir.path())
        .env(
            "RUST_LOG",
            std::env::var("JELLYSWARRM_TEST_RUST_LOG")
                .unwrap_or_else(|_| "jellyswarrm_proxy=warn".to_string()),
        )
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .with_context(|| format!("failed to start Jellyswarrm on port {proxy_port}"))?;
    Ok(ServerProcess {
        child,
        _data_dir: data_dir,
    })
}

async fn wait_for_proxy(client: &Client, base_url: &str, child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("Jellyswarrm exited before becoming ready: {status}");
        }
        if client
            .get(format!("{base_url}/System/Info/Public"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Jellyswarrm did not become ready within 30 seconds");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn login(client: &Client, base_url: &str, password: &str) -> Result<Response> {
    login_as(client, base_url, USERNAME, password, AUTHORIZATION).await
}

async fn login_as(
    client: &Client,
    base_url: &str,
    username: &str,
    password: &str,
    authorization: &str,
) -> Result<Response> {
    client
        .post(format!("{base_url}/Users/AuthenticateByName"))
        .header("Authorization", authorization)
        .json(&json!({"Username": username, "Pw": password}))
        .send()
        .await
        .context("login request failed")
}

async fn verify_seerr_integration(client: &Client, base_url: &str) -> Result<()> {
    let login =
        success_json(login_as(client, base_url, USERNAME, PASSWORD, SEERR_AUTHORIZATION).await?)
            .await?;
    assert_eq!(login["User"]["Policy"]["IsAdministrator"], true);
    let primary_token = required_string(&login, "/AccessToken")?;

    let created = seerr_authenticated(
        client
            .post(format!("{base_url}/Auth/Keys"))
            .query(&[("App", "Seerr")]),
        primary_token,
    )
    .send()
    .await?;
    assert_eq!(created.status(), StatusCode::NO_CONTENT);

    let keys = success_json(
        seerr_authenticated(client.get(format!("{base_url}/Auth/Keys")), primary_token)
            .send()
            .await?,
    )
    .await?;
    let scanner_token = items(&keys)?
        .iter()
        .rev()
        .find(|key| key["AppName"] == "Seerr")
        .and_then(|key| key["AccessToken"].as_str())
        .context("Seerr API key was not returned")?;
    assert_ne!(scanner_token, primary_token);

    let system_info = success_json(
        seerr_authenticated(client.get(format!("{base_url}/System/Info")), scanner_token)
            .send()
            .await?,
    )
    .await?;
    assert_eq!(system_info["ServerName"], "Jellyswarrm Proxy");

    let (libraries, movies) = wait_for_seerr_catalog(client, base_url, scanner_token).await?;
    let library_names = item_names(&libraries)?;
    assert!(library_names.contains("Movies"));
    assert!(library_names.contains("Shows"));
    let movie_names = item_names(&movies)?;
    assert!(
        expected_movie_names()
            .iter()
            .all(|name| movie_names.contains(name)),
        "Seerr movie catalog: {movie_names:?}"
    );

    let named_key_management =
        seerr_authenticated(client.get(format!("{base_url}/Auth/Keys")), scanner_token)
            .send()
            .await?;
    assert_eq!(named_key_management.status(), StatusCode::FORBIDDEN);

    let named_key_write = seerr_authenticated(
        client
            .delete(format!("{base_url}/Devices"))
            .query(&[("Id", "seerr-device")]),
        scanner_token,
    )
    .send()
    .await?;
    assert_eq!(named_key_write.status(), StatusCode::FORBIDDEN);

    let ordinary_login =
        success_json(login_as(client, base_url, USERNAME, PASSWORD, AUTHORIZATION).await?).await?;
    assert_eq!(ordinary_login["User"]["Policy"]["IsAdministrator"], false);

    Ok(())
}

async fn wait_for_seerr_catalog(
    client: &Client,
    base_url: &str,
    token: &str,
) -> Result<(Value, Value)> {
    let deadline = Instant::now() + CATALOG_TIMEOUT;
    loop {
        let response = seerr_authenticated(
            client.get(format!("{base_url}/Library/MediaFolders")),
            token,
        )
        .send()
        .await?;
        let last_observation = if response.status().is_success() {
            let libraries: Value = response.json().await?;
            let names = item_names(&libraries)?;
            let mut observation = format!("libraries: {names:?}");
            for movies_id in item_ids_named(&libraries, "Movies")? {
                let response = seerr_authenticated(
                    client.get(format!("{base_url}/Items")).query(&[
                        ("ParentId", movies_id),
                        ("Recursive", "true"),
                        ("IncludeItemTypes", "Series,Movie,Others"),
                        ("Fields", "ProviderIds,MediaSources,DateCreated"),
                    ]),
                    token,
                )
                .send()
                .await?;
                if response.status().is_success() {
                    let movies: Value = response.json().await?;
                    let movie_names = item_names(&movies)?;
                    observation = format!("libraries: {names:?}; movies: {movie_names:?}");
                    if expected_movie_names()
                        .iter()
                        .all(|name| movie_names.contains(name))
                    {
                        return Ok((libraries, movies));
                    }
                }
            }
            observation
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            format!("MediaFolders returned {status}: {body}")
        };

        if Instant::now() >= deadline {
            bail!(
                "Seerr catalog was not ready within {CATALOG_TIMEOUT:?}; last observation: {last_observation}"
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_views(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
) -> Result<Value> {
    let deadline = Instant::now() + CATALOG_TIMEOUT;
    loop {
        let response = authenticated(
            client.get(format!("{base_url}/Users/{user_id}/Views")),
            token,
        )
        .send()
        .await?;
        let last_observation = if response.status().is_success() {
            let views: Value = response.json().await?;
            let names = item_names(&views)?;
            if ["Movies", "Shows", "Music"]
                .iter()
                .all(|name| names.contains(name))
            {
                return Ok(views);
            }
            format!("views: {names:?}")
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            format!("Views returned {status}: {body}")
        };
        if Instant::now() >= deadline {
            bail!(
                "merged views were not ready within {CATALOG_TIMEOUT:?}; last observation: {last_observation}"
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_library_items(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    views: &Value,
    catalog: (&str, &str, &[&str]),
) -> Result<Value> {
    let (view_name, item_type, expected_names) = catalog;
    let deadline = Instant::now() + CATALOG_TIMEOUT;
    loop {
        let mut last_observation = format!("{view_name} view was not found");
        for view_id in item_ids_named(views, view_name)? {
            let response = authenticated(
                client
                    .get(format!("{base_url}/Users/{user_id}/Items"))
                    .query(&[
                        ("ParentId", view_id),
                        ("Recursive", "true"),
                        ("IncludeItemTypes", item_type),
                        ("Fields", "ProviderIds,MediaSources"),
                    ]),
                token,
            )
            .send()
            .await?;
            if response.status().is_success() {
                let items: Value = response.json().await?;
                let names = item_names(&items)?;
                last_observation = format!("{view_name} {view_id}: {names:?}");
                if expected_names.iter().all(|name| names.contains(name)) {
                    return Ok(items);
                }
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_observation = format!("{view_name} {view_id} returned {status}: {body}");
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "merged {view_name} catalog was not ready within {CATALOG_TIMEOUT:?}; last observation: {last_observation}"
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_merged_seasons(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    views: &Value,
) -> Result<(Value, String)> {
    let expected = HashSet::from(["Season 1", "Season 2", "Season 3"]);
    let deadline = Instant::now() + SHOW_TIMEOUT;
    loop {
        let mut last_observation = String::from("Shows view was not found");
        for view_id in item_ids_named(views, "Shows")? {
            let shows = fetch_items(
                client,
                base_url,
                user_id,
                token,
                view_id,
                "Series",
                "ProviderIds",
            )
            .await?;
            for series_id in item_ids_named(&shows, "One Step Beyond")? {
                let response = authenticated(
                    client
                        .get(format!("{base_url}/Users/{user_id}/Items"))
                        .query(&[
                            ("ParentId", series_id),
                            ("IncludeItemTypes", "Season"),
                            ("Fields", "ProviderIds"),
                        ]),
                    token,
                )
                .send()
                .await?;
                if response.status().is_success() {
                    let seasons: Value = response.json().await?;
                    let names = item_names(&seasons)?;
                    if names == expected {
                        return Ok((seasons, series_id.to_string()));
                    }
                    last_observation = format!("seasons of {series_id}: {names:?}");
                } else {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    last_observation = format!("Seasons {series_id} returned {status}: {body}");
                }
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "merged seasons were not ready within {SHOW_TIMEOUT:?}; last observation: {last_observation}"
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_merged_episode(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    views: &Value,
) -> Result<(Value, String)> {
    let deadline = Instant::now() + SHOW_TIMEOUT;
    loop {
        let mut last_observation = String::from("Shows view was not found");
        for view_id in item_ids_named(views, "Shows")? {
            let shows = fetch_items(
                client,
                base_url,
                user_id,
                token,
                view_id,
                "Series",
                "ProviderIds",
            )
            .await?;
            for series_id in item_ids_named(&shows, "One Step Beyond")? {
                let response = authenticated(
                    client
                        .get(format!("{base_url}/Users/{user_id}/Items"))
                        .query(&[
                            ("ParentId", series_id),
                            ("Recursive", "true"),
                            ("IncludeItemTypes", "Episode"),
                            ("Fields", "ProviderIds,MediaSources"),
                        ]),
                    token,
                )
                .send()
                .await?;
                if response.status().is_success() {
                    let episodes: Value = response.json().await?;
                    let all = items(&episodes)?;
                    let shared: Vec<&Value> = all
                        .iter()
                        .filter(|episode| {
                            episode["ParentIndexNumber"].as_i64() == Some(1)
                                && episode["IndexNumber"].as_i64() == Some(2)
                        })
                        .collect();
                    if all.len() == 3
                        && shared.len() == 1
                        && shared[0]["MediaSourceCount"].as_i64() == Some(2)
                    {
                        return Ok((shared[0].clone(), series_id.to_string()));
                    }
                    last_observation = format!(
                        "episodes of {series_id}: {:?}",
                        all.iter()
                            .map(|episode| (
                                episode["ParentIndexNumber"].as_i64(),
                                episode["IndexNumber"].as_i64(),
                                episode["MediaSourceCount"].as_i64(),
                            ))
                            .collect::<Vec<_>>()
                    );
                } else {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    last_observation = format!("Episodes {series_id} returned {status}: {body}");
                }
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "merged episode was not ready within {SHOW_TIMEOUT:?}; last observation: {last_observation}"
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn fetch_items(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    view_id: &str,
    item_type: &str,
    fields: &str,
) -> Result<Value> {
    success_json(
        authenticated(
            client
                .get(format!("{base_url}/Users/{user_id}/Items"))
                .query(&[
                    ("ParentId", view_id),
                    ("Recursive", "true"),
                    ("IncludeItemTypes", item_type),
                    ("Fields", fields),
                ]),
            token,
        )
        .send()
        .await?,
    )
    .await
}

fn expected_movie_names() -> [&'static str; 5] {
    [
        "Big Buck Bunny [Movies 1]",
        "Big Buck Bunny [Movies 2]",
        "Night of the Living Dead",
        "Plan 9 from Outer Space",
        "Sintel",
    ]
}

async fn verify_playback(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    item_id: &str,
) -> Result<()> {
    verify_playback_source(client, base_url, user_id, token, item_id, None).await
}

async fn verify_version_switching(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    aggregate_id: &str,
) -> Result<()> {
    let detail = fetch_user_item(client, base_url, user_id, token, aggregate_id).await?;
    let sources = detail["MediaSources"]
        .as_array()
        .context("missing versions")?;
    assert_eq!(sources.len(), 2);
    let source_ids = sources
        .iter()
        .map(|source| required_string(source, "/Id"))
        .collect::<Result<HashSet<_>>>()?;
    // The details UI reloads by source ID, without Fields or aggregate context.
    for index in [1, 0, 1] {
        let source_id = required_string(&sources[index], "/Id")?;
        let selected = success_json(
            authenticated(
                client
                    .get(format!("{base_url}/Items/{source_id}"))
                    .query(&[("userId", user_id)]),
                token,
            )
            .send()
            .await?,
        )
        .await?;
        assert_eq!(
            required_string(&selected, "/Id")?,
            source_id,
            "selecting a version must retain its item identity"
        );
        assert_ne!(source_id, aggregate_id);
        assert_eq!(selected["MediaSourceCount"], 2);
        let selected_sources = selected["MediaSources"]
            .as_array()
            .context("selected detail lost versions")?;
        assert_eq!(
            selected_sources
                .iter()
                .map(|source| required_string(source, "/Id"))
                .collect::<Result<HashSet<_>>>()?,
            source_ids,
            "switching versions must retain both selector options"
        );
        assert_eq!(selected_sources[0]["Id"], source_id);
        assert_eq!(selected_sources[0]["Path"], sources[index]["Path"]);
        if selected["Type"] == "Episode" {
            assert_eq!(selected["SeriesId"], detail["SeriesId"]);
            assert_eq!(selected["SeasonId"], detail["SeasonId"]);
            assert_eq!(selected["ParentIndexNumber"], detail["ParentIndexNumber"]);
            assert_eq!(selected["IndexNumber"], detail["IndexNumber"]);
            let series_id = required_string(&selected, "/SeriesId")?;
            let season_id = required_string(&selected, "/SeasonId")?;
            // Episode details reloads "More from season" after a version switch.
            let more = success_json(
                authenticated(
                    client
                        .get(format!("{base_url}/Shows/{series_id}/Episodes"))
                        .query(&[
                            ("UserId", user_id),
                            ("SeasonId", season_id),
                            (
                                "Fields",
                                "ItemCounts,PrimaryImageAspectRatio,CanDelete,MediaSourceCount",
                            ),
                        ]),
                    token,
                )
                .send()
                .await?,
            )
            .await?;
            let more_items = items(&more)?;
            assert_eq!(more_items.len(), 1);
            assert_eq!(more_items[0]["Id"], aggregate_id);
            assert_eq!(more_items[0]["MediaSourceCount"], 2);
        }
        verify_playback_source(client, base_url, user_id, token, source_id, Some(source_id))
            .await?;
    }
    Ok(())
}

async fn verify_playback_source(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    item_id: &str,
    selected_source: Option<&str>,
) -> Result<()> {
    let mut payload = json!({"UserId": user_id, "IsPlayback": true});
    if let Some(source) = selected_source {
        payload["MediaSourceId"] = json!(source);
    }
    let playback = authenticated(
        client
            .post(format!("{base_url}/Items/{item_id}/PlaybackInfo"))
            .query(&[("UserId", user_id)])
            .json(&payload),
        token,
    )
    .send()
    .await?;
    let playback = success_json(playback).await?;
    let play_session_id = required_string(&playback, "/PlaySessionId")?;
    let media_source_id = required_string(&playback, "/MediaSources/0/Id")?;
    if let Some(selected_source) = selected_source {
        assert_eq!(
            media_source_id, selected_source,
            "playback must use the selected version"
        );
    }

    let stream = authenticated(
        client
            .get(format!("{base_url}/Videos/{item_id}/stream.mp4"))
            .query(&[
                ("Static", "true"),
                ("MediaSourceId", media_source_id),
                ("PlaySessionId", play_session_id),
            ])
            .header("Range", "bytes=0-4095"),
        token,
    )
    .send()
    .await?;
    let status = stream.status();
    let bytes = stream.bytes().await?;
    assert!(
        status == StatusCode::PARTIAL_CONTENT || status == StatusCode::OK,
        "unexpected stream status {status}"
    );
    assert!(
        bytes.len() >= 1024,
        "stream returned only {} bytes",
        bytes.len()
    );
    assert!(
        bytes.windows(4).any(|window| window == b"ftyp"),
        "stream does not begin with an MP4 file header"
    );
    Ok(())
}

// Exercise persisted Jellyfin state as well as proxy translation. The fixture
// puts Aria on Music 1 and Death Valley Waltz on Music 2.
async fn verify_saved_playlist(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    music: &Value,
    music_upstream: &str,
    duplicate_song: bool,
) -> Result<()> {
    let song = item_id_named(music, "01 - Aria")?;
    let added_song = if duplicate_song {
        song
    } else {
        item_id_named(music, "02 - Variatio 1 a 1 Clav")?
    };
    let foreign_song = item_id_named(music, "01 - Death Valley Waltz")?;
    let created = success_json(authenticated(client.post(format!("{base_url}/Playlists"))
        .json(&json!({"Name":"Integration saved playlist", "MediaType":"Audio", "Ids":[song], "UserId":user_id})), token)
        .send().await?).await?;
    let playlist = required_string(&created, "/Id")?;
    let playlist_url = format!("{base_url}/Playlists/{playlist}");
    let entries_url = format!("{playlist_url}/Items");
    let read = || {
        authenticated(
            client.get(&entries_url).query(&[("UserId", user_id)]),
            token,
        )
        .send()
    };
    let initial = success_json(read().await?).await?;
    assert_eq!(items(&initial)?.len(), 1);
    assert_eq!(items(&initial)?[0]["Id"], song);

    success_text(
        authenticated(
            client
                .post(&entries_url)
                .query(&[("Ids", added_song), ("UserId", user_id)]),
            token,
        )
        .send()
        .await?,
    )
    .await?;
    let duplicated = success_json(read().await?).await?;
    let entries = items(&duplicated)?;
    assert_eq!(entries.len(), 2, "adding a song must retain both entries");
    assert_eq!(entries[0]["Id"], song);
    assert_eq!(entries[1]["Id"], added_song);
    let first = required_string(&entries[0], "/PlaylistItemId")?;
    let second = required_string(&entries[1], "/PlaylistItemId")?;
    if first == second {
        let upstream_login = success_json(
            login_as(client, music_upstream, "admin", "password", AUTHORIZATION).await?,
        )
        .await
        .context("direct upstream login")?;
        let upstream_token = required_string(&upstream_login, "/AccessToken")?;
        let playlists = success_json(
            client
                .get(format!("{music_upstream}/Items"))
                .header(
                    "Authorization",
                    format!("{AUTHORIZATION}, Token=\"{upstream_token}\""),
                )
                .query(&[("Recursive", "true"), ("IncludeItemTypes", "Playlist")])
                .send()
                .await?,
        )
        .await
        .context("direct playlist lookup")?;
        let original_playlist = item_id_named(&playlists, "Integration saved playlist")?;
        let original_entries = success_json(
            client
                .get(format!(
                    "{music_upstream}/Playlists/{original_playlist}/Items"
                ))
                .header(
                    "Authorization",
                    format!("{AUTHORIZATION}, Token=\"{upstream_token}\""),
                )
                .send()
                .await?,
        )
        .await
        .context("direct playlist entries")?;
        let upstream_ids: Vec<_> = items(&original_entries)?
            .iter()
            .map(|item| item["PlaylistItemId"].clone())
            .collect();
        bail!("duplicate songs cannot be addressed independently: proxy entry IDs are [{first}, {second}]; direct Jellyfin entry IDs are {upstream_ids:?}");
    }
    success_text(
        authenticated(client.post(format!("{entries_url}/{second}/Move/0")), token)
            .send()
            .await?,
    )
    .await?;
    let reordered = success_json(read().await?).await?;
    assert_eq!(items(&reordered)?[0]["PlaylistItemId"], second);
    assert_eq!(items(&reordered)?[1]["PlaylistItemId"], first);

    for (url, body) in [
        (
            format!("{base_url}/Playlists"),
            Some(
                json!({"Name":"Unsupported mixed playlist", "MediaType":"Audio", "Ids":[song, foreign_song], "UserId":user_id}),
            ),
        ),
        (
            format!(
                "{base_url}/Playlists?Name=Unsupported&Ids={song},{foreign_song}&UserId={user_id}"
            ),
            None,
        ),
        (
            format!("{entries_url}?Ids={foreign_song}&UserId={user_id}"),
            None,
        ),
        (entries_url.clone(), Some(json!({"Ids":[foreign_song]}))),
    ] {
        let mut request = authenticated(client.post(url), token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response
            .text()
            .await?
            .contains("mixed-server playlists are unsupported"));
    }
    let unchanged = success_json(read().await?).await?;
    assert_eq!(
        items(&unchanged)?.len(),
        2,
        "rejected adds must leave saved state unchanged"
    );
    assert_eq!(items(&unchanged)?[0]["PlaylistItemId"], second);
    assert_eq!(items(&unchanged)?[1]["PlaylistItemId"], first);

    // Log in a different fixture user so the proxy has their target-server mapping.
    let recipient =
        success_json(login_as(client, base_url, "admin", "password", AUTHORIZATION).await?).await?;
    let recipient_id = required_string(&recipient, "/User/Id")?;
    assert_ne!(recipient_id, user_id);
    let users_url = format!("{playlist_url}/Users");
    let before_share =
        success_json(authenticated(client.get(&users_url), token).send().await?).await?;
    success_text(
        authenticated(
            client
                .post(format!("{users_url}/{recipient_id}"))
                .json(&json!({"CanEdit":true})),
            token,
        )
        .send()
        .await?,
    )
    .await?;
    let shared = success_json(authenticated(client.get(&users_url), token).send().await?).await?;
    assert_eq!(
        shared
            .as_array()
            .context("playlist users must be an array")?
            .len(),
        before_share
            .as_array()
            .context("playlist users must be an array")?
            .len()
            + 1,
        "sharing must add the recipient rather than update the owner"
    );
    success_text(
        authenticated(client.delete(format!("{users_url}/{recipient_id}")), token)
            .send()
            .await?,
    )
    .await?;
    let unshared = success_json(authenticated(client.get(&users_url), token).send().await?).await?;
    let sharing_was_removed = unshared == before_share;

    success_text(
        authenticated(
            client.delete(&entries_url).query(&[("EntryIds", second)]),
            token,
        )
        .send()
        .await?,
    )
    .await?;
    let removed = success_json(read().await?).await?;
    assert_eq!(items(&removed)?.len(), 1);
    assert_eq!(items(&removed)?[0]["Id"], song);
    assert_eq!(
        items(&removed)?[0]["PlaylistItemId"],
        first,
        "removing one duplicate must preserve the other entry"
    );
    let detail = success_json(
        authenticated(
            client
                .get(format!("{base_url}/Users/{user_id}/Items/{playlist}"))
                .query(&[("Fields", "CanDelete")]),
            token,
        )
        .send()
        .await?,
    )
    .await?;
    assert_eq!(detail["CanDelete"], true);
    success_text(
        authenticated(client.delete(format!("{base_url}/Items/{playlist}")), token)
            .send()
            .await?,
    )
    .await?;
    let deleted = authenticated(
        client.get(format!("{base_url}/Users/{user_id}/Items/{playlist}")),
        token,
    )
    .send()
    .await?;
    assert_eq!(deleted.status(), StatusCode::NOT_FOUND);
    if !sharing_was_removed {
        bail!("removing the sharing recipient returned success but persisted users were {unshared}; expected {before_share}");
    }
    Ok(())
}

async fn verify_audio_playback(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    track: &Value,
) -> Result<()> {
    let item_id = required_string(track, "/Id")?;
    let album_id = required_string(track, "/AlbumId")?;
    let track = fetch_user_item(client, base_url, user_id, token, item_id).await?;
    assert_eq!(required_string(&track, "/Id")?, item_id);
    assert_eq!(required_string(&track, "/AlbumId")?, album_id);

    let album = fetch_user_item(client, base_url, user_id, token, album_id).await?;
    assert_eq!(required_string(&album, "/Id")?, album_id);
    assert_eq!(required_string(&album, "/Type")?, "MusicAlbum");
    assert!(required_string(&album, "/Name")?.contains("Ghost Solos"));

    let master = authenticated(
        client
            .get(format!("{base_url}/Audio/{item_id}/universal"))
            .query(&[
                ("UserId", user_id),
                ("DeviceId", "jellyswarrm-integration-tests"),
                ("MaxStreamingBitrate", "128000"),
                ("Container", "mp3"),
                ("TranscodingContainer", "ts"),
                ("TranscodingProtocol", "hls"),
                ("AudioCodec", "aac"),
                ("PlaySessionId", "client-generated-audio-session"),
                ("StartTimeTicks", "0"),
                ("EnableRedirection", "true"),
                ("EnableRemoteMedia", "false"),
            ]),
        token,
    )
    .send()
    .await?;
    let master_url = master.url().clone();
    let master = success_text(master).await?;
    assert!(master.starts_with("#EXTM3U"), "invalid HLS master playlist");

    let media_url = playlist_entry(&master_url, &master)?;
    let media = authenticated(client.get(media_url.clone()), token)
        .send()
        .await?;
    let media = success_text(media).await?;
    assert!(media.starts_with("#EXTM3U"), "invalid HLS media playlist");

    let segment_url = playlist_entry(&media_url, &media)?;
    assert!(
        segment_url.path().contains("/hls1/"),
        "unexpected HLS segment URL: {segment_url}"
    );
    let segment = authenticated(client.get(segment_url), token).send().await?;
    let status = segment.status();
    let bytes = segment.bytes().await?;
    assert!(status.is_success(), "HLS segment returned {status}");
    assert!(
        bytes.len() >= 1024,
        "HLS segment returned {} bytes",
        bytes.len()
    );
    assert_eq!(bytes.first(), Some(&0x47), "HLS segment is not MPEG-TS");
    Ok(())
}

async fn fetch_user_item(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    item_id: &str,
) -> Result<Value> {
    success_json(
        authenticated(
            client.get(format!("{base_url}/Users/{user_id}/Items/{item_id}")),
            token,
        )
        .send()
        .await?,
    )
    .await
}

fn playlist_entry(base_url: &Url, playlist: &str) -> Result<Url> {
    let entry = playlist
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .context("playlist did not contain a media URI")?;
    base_url
        .join(entry)
        .with_context(|| format!("invalid playlist URI {entry}"))
}

fn authenticated(builder: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    builder.header("X-Emby-Token", token)
}

fn seerr_authenticated(builder: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    builder.header(
        "Authorization",
        format!("{SEERR_AUTHORIZATION}, Token=\"{token}\""),
    )
}

async fn success_json(response: Response) -> Result<Value> {
    let body = success_text(response).await?;
    serde_json::from_str(&body).with_context(|| format!("invalid JSON response: {body}"))
}

async fn success_text(response: Response) -> Result<String> {
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        bail!("request failed with {status}: {body}");
    }
    Ok(body)
}

fn items(payload: &Value) -> Result<&[Value]> {
    payload["Items"]
        .as_array()
        .map(Vec::as_slice)
        .context("response did not contain an Items array")
}

fn item_names(payload: &Value) -> Result<HashSet<&str>> {
    Ok(items(payload)?
        .iter()
        .filter_map(|item| item["Name"].as_str())
        .collect())
}

fn item_id_named<'a>(payload: &'a Value, name: &str) -> Result<&'a str> {
    item_named(payload, name)?
        .get("Id")
        .and_then(Value::as_str)
        .with_context(|| format!("response did not contain an item named {name}"))
}

fn item_named<'a>(payload: &'a Value, name: &str) -> Result<&'a Value> {
    items(payload)?
        .iter()
        .find(|item| item["Name"] == name)
        .with_context(|| format!("response did not contain an item named {name}"))
}

fn item_ids_named<'a>(payload: &'a Value, name: &str) -> Result<Vec<&'a str>> {
    Ok(items(payload)?
        .iter()
        .filter(|item| item["Name"] == name)
        .filter_map(|item| item["Id"].as_str())
        .collect())
}

fn required_string<'a>(payload: &'a Value, pointer: &str) -> Result<&'a str> {
    payload
        .pointer(pointer)
        .and_then(Value::as_str)
        .with_context(|| format!("response did not contain string field {pointer}"))
}
