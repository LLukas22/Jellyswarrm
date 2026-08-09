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

struct ServerProcess {
    child: Child,
    _data_dir: TempDir,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker and the Git LFS media fixtures"]
async fn user_can_login_browse_merged_libraries_and_stream_mapped_media() -> Result<()> {
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
    write_proxy_config(data_dir.path(), proxy_port, &upstreams)?;
    let mut proxy = start_proxy(data_dir, proxy_port)?;
    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
    wait_for_proxy(&client, &proxy_url, &mut proxy.child).await?;

    let bad_login = login(&client, &proxy_url, "wrong-password").await?;
    assert_eq!(bad_login.status(), StatusCode::UNAUTHORIZED);

    let login = success_json(login(&client, &proxy_url, PASSWORD).await?).await?;
    let token = required_string(&login, "/AccessToken")?;
    let user_id = required_string(&login, "/User/Id")?;
    assert_eq!(required_string(&login, "/User/Name")?, USERNAME);

    let views = wait_for_views(&client, &proxy_url, user_id, token).await?;
    let movies = wait_for_library_items(
        &client,
        &proxy_url,
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
        verify_playback(&client, &proxy_url, user_id, token, item_id)
            .await
            .with_context(|| format!("failed playback check for {movie_name}"))?;
    }

    let music = wait_for_library_items(
        &client,
        &proxy_url,
        user_id,
        token,
        &views,
        ("Music", "Audio", &["01 - Aria", "01 - Death Valley Waltz"]),
    )
    .await?;
    verify_hls_audio(
        &client,
        &proxy_url,
        user_id,
        token,
        item_id_named(&music, "01 - Death Valley Waltz")?,
    )
    .await
    .context("failed HLS playback check for Death Valley Waltz")?;
    verify_seerr_integration(&client, &proxy_url).await?;

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
) -> Result<()> {
    let mut config = format!(
        "host = \"127.0.0.1\"\nport = {proxy_port}\ninclude_server_name_in_media = false\nmerge_libraries = true\nserver_background_check_interval_secs = 1\n"
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
        .env("RUST_LOG", "jellyswarrm_proxy=warn")
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
    let playback = authenticated(
        client
            .post(format!("{base_url}/Items/{item_id}/PlaybackInfo"))
            .query(&[("UserId", user_id)])
            .json(&json!({"UserId": user_id, "IsPlayback": true})),
        token,
    )
    .send()
    .await?;
    let playback = success_json(playback).await?;
    let play_session_id = required_string(&playback, "/PlaySessionId")?;
    let media_source_id = required_string(&playback, "/MediaSources/0/Id")?;

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

async fn verify_hls_audio(
    client: &Client,
    base_url: &str,
    user_id: &str,
    token: &str,
    item_id: &str,
) -> Result<()> {
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
    items(payload)?
        .iter()
        .find(|item| item["Name"] == name)
        .and_then(|item| item["Id"].as_str())
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
