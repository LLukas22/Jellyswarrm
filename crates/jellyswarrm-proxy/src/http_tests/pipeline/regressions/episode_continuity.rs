//! Default episode selection follows this client's current server, not another
//! client's playback or the library's highest-priority representative.
use super::*;

const SERIES: &str = "44444444444444444444444444444444";

fn episode(number: usize) -> String {
    format!("{:032x}", 100 + number)
}

async fn fixture() -> (Fixture, Vec<String>, Vec<String>) {
    let f = Fixture::new().await;
    f.state.config.write().await.deduplicate_media = true;
    healthy(&f).await;
    for (index, upstream) in f.upstreams.iter().enumerate() {
        Mock::given(method("GET")).and(path("/Items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": SERIES, "Name": "Show", "Type": "Series", "ProviderIds": {"Tvdb": "42"}}],
                "TotalRecordCount": 1, "StartIndex": 0
            }))).mount(upstream).await;
        let numbers = if index == 0 {
            vec![1, 2, 3, 4]
        } else {
            vec![1, 2, 4]
        };
        let items = numbers
            .iter()
            .map(|number| {
                json!({
                    "Id": episode(*number), "Name": format!("Episode {number}"), "Type": "Episode",
                    "SeriesId": SERIES, "ParentIndexNumber": if *number == 4 { 2 } else { 1 },
                    "IndexNumber": if *number == 4 { 1 } else { *number },
                    "MediaSources": [{"Id": episode(*number), "Type": "Default"}]
                })
            })
            .collect::<Vec<_>>();
        Mock::given(method("GET"))
            .and(path(format!("/Shows/{SERIES}/Episodes")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "TotalRecordCount": items.len(), "StartIndex": 0, "Items": items
            })))
            .mount(upstream)
            .await;
        for number in numbers {
            Mock::given(method("GET"))
                .and(path(format!("/Items/{}", episode(number))))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "Id": episode(number), "Type": "Episode", "Name": format!("Episode {number}"),
                    "SeriesId": SERIES, "MediaSources": [{"Id": episode(number), "Type": "Default"}]
                })))
                .mount(upstream)
                .await;
            Mock::given(method("POST"))
                .and(path(format!("/Items/{}/PlaybackInfo", episode(number))))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "PlaySessionId": format!("server-{index}-episode-{number}"),
                    "MediaSources": [{"Id": episode(number), "Type": "Default", "SupportsDirectPlay": true}]
                }))).mount(upstream).await;
        }
        for path in ["/Sessions/Playing", "/Sessions/Playing/Stopped"] {
            Mock::given(method("POST"))
                .and(wiremock::matchers::path(path))
                .respond_with(ResponseTemplate::new(204))
                .mount(upstream)
                .await;
        }
    }
    let shows = catalog(&f, "/Items?SearchTerm=Show").await;
    let series = shows["Items"][0]["Id"].as_str().unwrap();
    let episodes = catalog(&f, &format!("/Shows/{series}/Episodes?Fields=MediaSources")).await;
    let items = episodes["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(items.len(), 4);
    let mut secondary_sources = Vec::new();
    for item in &items {
        catalog(&f, &format!("/Items/{item}")).await;
        let member = f
            .state
            .media_storage
            .get_media_version_members_by_virtual_id(item)
            .await
            .unwrap()
            .into_iter()
            .find(|member| {
                member.server.url.as_str().trim_end_matches('/') == f.upstreams[1].uri()
            });
        secondary_sources.push(
            member
                .map(|member| member.mapping.virtual_media_id)
                .unwrap_or_default(),
        );
    }
    (f, items, secondary_sources)
}

async fn playback(
    f: &Fixture,
    item: &str,
    device: &str,
    source: Option<&str>,
    expected_server: usize,
    number: usize,
) {
    let mut body = json!({"IsPlayback": true, "DeviceProfile": {}});
    if let Some(source) = source {
        body["MediaSourceId"] = json!(source);
    }
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{item}/PlaybackInfo"),
            &f.user,
            device,
        )
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.unwrap();
    assert_eq!(
        response["PlaySessionId"],
        format!("server-{expected_server}-episode-{number}")
    );
}

async fn add_device(f: &Fixture, device: &str) {
    let sessions = f
        .state
        .user_authorization
        .get_user_sessions(&f.user.id, None)
        .await
        .unwrap();
    for (session, server) in sessions {
        f.state
            .user_authorization
            .store_authorization_session(
                &f.user.id,
                &server,
                &Authorization::parse(&auth(&f.user, device)).unwrap(),
                session.jellyfin_token,
                session.original_user_id,
                None,
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn next_episode_prefers_current_server_and_follows_the_fallback_across_seasons() {
    let (f, items, sources) = fixture().await;
    playback(&f, &items[0], "tv", Some(&sources[0]), 1, 1).await;
    // Stopping releases the old playback binding, not the client's preference.
    let stopped = f.request(reqwest::Method::POST, "/Sessions/Playing/Stopped", &f.user, "tv")
        .json(&json!({"ItemId":items[0], "PlaySessionId":"server-1-episode-1", "MediaSourceId":sources[0]}))
        .send().await.unwrap();
    assert_eq!(stopped.status(), StatusCode::NO_CONTENT);
    playback(&f, &items[1], "tv", None, 1, 2).await;
    // Jellyfin Web's remembered-track sentinel is still an implicit selection.
    playback(&f, &items[1], "tv", Some(&items[1]), 1, 2).await;
    let response = f
        .request(
            reqwest::Method::POST,
            &format!(
                "/Items/{}/PlaybackInfo?MediaSourceId={}",
                items[1], items[1]
            ),
            &f.user,
            "tv",
        )
        .json(&json!({"IsPlayback":true,"DeviceProfile":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["PlaySessionId"],
        "server-1-episode-2"
    );
    // Episode 3 only exists on server 0. Episode 4 exists on both: stay on 0.
    playback(&f, &items[2], "tv", None, 0, 3).await;
    playback(&f, &items[3], "tv", None, 0, 4).await;
    // An explicit selection can switch back to the other copy.
    playback(&f, &items[3], "tv", Some(&sources[3]), 1, 4).await;
}

#[tokio::test]
async fn episode_server_preference_is_isolated_by_device_and_viewer() {
    let (f, items, sources) = fixture().await;
    playback(&f, &items[0], "tv", Some(&sources[0]), 1, 1).await;
    add_device(&f, "phone").await;
    playback(&f, &items[1], "phone", None, 0, 2).await;
    playback(&f, &items[1], "tv", None, 1, 2).await;

    // The other viewer sees the same catalog without inheriting this client's route.
    let sessions = f
        .state
        .user_authorization
        .get_user_sessions(&f.user.id, None)
        .await
        .unwrap();
    for (session, server) in sessions
        .into_iter()
        .filter(|(session, _)| session.device.device_id == "tv")
    {
        f.state
            .user_authorization
            .add_server_mapping(&f.other.id, &server, "bob", &"password".into(), None)
            .await
            .unwrap();
        f.state
            .user_authorization
            .store_authorization_session(
                &f.other.id,
                &server,
                &Authorization::parse(&auth(&f.other, "tv")).unwrap(),
                session.jellyfin_token,
                session.original_user_id,
                None,
            )
            .await
            .unwrap();
    }
    let shows: Value = f
        .request(
            reqwest::Method::GET,
            "/Items?SearchTerm=Show",
            &f.other,
            "tv",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let series = shows["Items"][0]["Id"].as_str().unwrap();
    let episodes: Value = f
        .request(
            reqwest::Method::GET,
            &format!("/Shows/{series}/Episodes"),
            &f.other,
            "tv",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let item = episodes["Items"][1]["Id"].as_str().unwrap();
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{item}/PlaybackInfo"),
            &f.other,
            "tv",
        )
        .json(&json!({"IsPlayback":true,"DeviceProfile":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["PlaySessionId"],
        "server-0-episode-2"
    );
    playback(&f, &items[1], "tv", None, 1, 2).await;
}

#[tokio::test]
async fn offline_episode_preference_falls_back_but_explicit_source_does_not() {
    let (f, items, sources) = fixture().await;
    playback(&f, &items[0], "tv", Some(&sources[0]), 1, 1).await;
    Mock::given(path("/System/Info/Public"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&f.upstreams[1])
        .await;
    f.state.server_storage.check_servers_health().await;
    playback(&f, &items[1], "tv", None, 0, 2).await;
    let before = f.upstreams[0].received_requests().await.unwrap().len();
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{}/PlaybackInfo", items[1]),
            &f.user,
            "tv",
        )
        .json(&json!({"IsPlayback":true,"MediaSourceId":sources[1],"DeviceProfile":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        f.upstreams[0].received_requests().await.unwrap().len(),
        before
    );
}

#[tokio::test]
async fn failed_or_preview_negotiation_does_not_change_episode_server_preference() {
    let (f, items, sources) = fixture().await;
    playback(&f, &items[0], "tv", Some(&sources[0]), 1, 1).await;
    let first_source = f
        .state
        .media_storage
        .get_media_version_members_by_virtual_id(&items[1])
        .await
        .unwrap()
        .into_iter()
        .find(|member| member.server.url.as_str().trim_end_matches('/') == f.upstreams[0].uri())
        .unwrap()
        .mapping
        .virtual_media_id;
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{}/PlaybackInfo", items[1]),
            &f.user,
            "tv",
        )
        .json(&json!({"IsPlayback":false,"MediaSourceId":first_source,"DeviceProfile":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    playback(&f, &items[1], "tv", None, 1, 2).await;

    Mock::given(method("POST"))
        .and(path(format!("/Items/{}/PlaybackInfo", episode(3))))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&f.upstreams[0])
        .await;
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{}/PlaybackInfo", items[2]),
            &f.user,
            "tv",
        )
        .json(&json!({"IsPlayback":true,"DeviceProfile":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    playback(&f, &items[3], "tv", None, 1, 4).await;
}

#[tokio::test]
async fn successful_playback_report_establishes_preference_for_clients_omitting_is_playback() {
    let (f, items, sources) = fixture().await;
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/Items/{}/PlaybackInfo", items[0]),
            &f.user,
            "tv",
        )
        .json(&json!({"MediaSourceId":sources[0],"DeviceProfile":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = f.request(reqwest::Method::POST, "/Sessions/Playing", &f.user, "tv")
        .json(&json!({"ItemId":items[0],"MediaSourceId":sources[0],"PlaySessionId":"server-1-episode-1"}))
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    playback(&f, &items[1], "tv", None, 1, 2).await;
}
