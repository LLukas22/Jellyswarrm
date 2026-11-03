//! The unmodified web client discovers its selected episode inside this queue.
use super::*;

const SERIES: &str = "44444444444444444444444444444444";

fn episode(number: usize) -> String {
    format!("{:032x}", 1000 + number)
}

fn web_auth(f: &Fixture) -> Authorization {
    let mut authorization = Authorization::parse(&auth(&f.user, "tv")).unwrap();
    authorization.client = "Jellyfin Web".into();
    authorization
}

fn queue_url(series: &str, item: &str) -> String {
    format!("/Shows/{series}/Episodes?IsVirtualUnaired=false&IsMissing=false&Fields=Chapters,MediaSources,Trickplay&limit=100&startItemId={item}")
}

async fn secondary_show(count: usize) -> (Fixture, String, Vec<String>) {
    let f = Fixture::new().await;
    healthy(&f).await;
    let authorization = web_auth(&f);
    for (session, server) in f
        .state
        .user_authorization
        .get_user_sessions(&f.user.id, None)
        .await
        .unwrap()
    {
        f.state
            .user_authorization
            .store_authorization_session(
                &f.user.id,
                &server,
                &authorization,
                session.jellyfin_token.clone(),
                session.original_user_id.clone(),
                None,
            )
            .await
            .unwrap();
        let mut native = authorization.clone();
        native.client = "Test".into();
        native.device_id = "native".into();
        f.state
            .user_authorization
            .store_authorization_session(
                &f.user.id,
                &server,
                &native,
                session.jellyfin_token,
                session.original_user_id,
                None,
            )
            .await
            .unwrap();
    }
    let server = f
        .state
        .server_storage
        .list_servers()
        .await
        .unwrap()
        .into_iter()
        .find(|server| server.url.as_str().trim_end_matches('/') == f.upstreams[1].uri())
        .unwrap();
    let series = f
        .state
        .media_storage
        .get_or_create_media_mapping(SERIES, &server)
        .await
        .unwrap()
        .virtual_media_id;
    let mut ids = Vec::new();
    for number in 0..count {
        ids.push(
            f.state
                .media_storage
                .get_or_create_media_mapping(&episode(number), &server)
                .await
                .unwrap()
                .virtual_media_id,
        );
    }
    Mock::given(method("GET"))
        .and(path(format!("/Shows/{SERIES}/Episodes")))
        .respond_with(move |request: &wiremock::Request| {
            let query = |key: &str| {
                request
                    .url
                    .query_pairs()
                    .find(|(name, _)| name.eq_ignore_ascii_case(key))
                    .map(|(_, value)| value.into_owned())
            };
            let anchor = query("StartItemId")
                .and_then(|id| (0..count).find(|number| episode(*number) == id))
                .unwrap_or(0);
            let offset = query("StartIndex")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            // Real backends may cap even a request for 100 items.
            let limit = query("Limit")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(count)
                .min(37);
            let items = (anchor + offset..count)
                .take(limit)
                .map(|number| {
                    json!({
                        "Id": episode(number), "Type": "Episode", "SeriesId": SERIES,
                        "Name": format!("Episode {number}"), "MediaType": "Video",
                        "ParentIndexNumber": number / 20 + 1, "IndexNumber": number % 20 + 1,
                        "MediaSources": [{"Id": episode(number), "Type": "Default"}]
                    })
                })
                .collect::<Vec<_>>();
            ResponseTemplate::new(200).set_body_json(json!({
                "Items": items, "StartIndex": offset, "TotalRecordCount": count - anchor
            }))
        })
        .mount(&f.upstreams[1])
        .await;
    (f, series, ids)
}

#[tokio::test]
async fn web_episode_queue_includes_previous_episodes_on_secondary_server() {
    let (f, series, ids) = secondary_show(320).await;
    for number in [0, 1, 36, 99, 100, 319] {
        let response = reqwest::Client::new()
            .get(format!("{}{}", f.url, queue_url(&series, &ids[number])))
            .header("Authorization", web_auth(&f).to_header_value())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result: Value = response.json().await.unwrap();
        let items = result["Items"].as_array().unwrap();
        assert!(items.len() <= 100);
        let selected = items
            .iter()
            .position(|item| item["Id"] == ids[number])
            .expect("the stock web client must find the selected episode");
        if number > 0 {
            assert!(
                selected > 0,
                "episode {number} must retain a previous episode"
            );
            assert_eq!(items[selected - 1]["Id"], ids[number - 1]);
        }
        if number + 1 < ids.len() {
            assert_eq!(items[selected + 1]["Id"], ids[number + 1]);
        }
        assert!(items.iter().all(|item| item["SeriesId"] == series));
    }
    assert!(
        f.upstreams[0]
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.url.path().ends_with("/Episodes")),
        "never send this series to the main server"
    );
}

#[tokio::test]
async fn ordinary_episode_anchors_and_explicit_pages_keep_jellyfin_semantics() {
    let (f, series, ids) = secondary_show(4).await;
    for (client, suffix, expected) in [
        ("Test", "", vec![ids[2].clone(), ids[3].clone()]),
        ("Jellyfin Web", "&StartIndex=1", vec![ids[3].clone()]),
    ] {
        let mut authorization = web_auth(&f);
        authorization.client = client.into();
        if client == "Test" {
            authorization.device_id = "native".into();
        }
        let response = reqwest::Client::new()
            .get(format!("{}{}{suffix}", f.url, queue_url(&series, &ids[2])))
            .header("Authorization", authorization.to_header_value())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result: Value = response.json().await.unwrap();
        assert_eq!(
            result["Items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["Id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

async fn web_catalog(f: &Fixture, path: &str) -> Value {
    let response = reqwest::Client::new()
        .get(format!("{}{path}", f.url))
        .header("Authorization", web_auth(f).to_header_value())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

#[tokio::test]
async fn web_episode_queue_handles_short_series_and_unknown_anchors() {
    for count in [1, 3] {
        let (f, series, ids) = secondary_show(count).await;
        let last = web_catalog(&f, &queue_url(&series, ids.last().unwrap())).await;
        assert_eq!(last["Items"].as_array().unwrap().len(), count);
        assert_eq!(last["Items"][count - 1]["Id"], ids[count - 1]);
        let missing =
            web_catalog(&f, &queue_url(&series, "ffffffffffffffffffffffffffffffff")).await;
        assert_eq!(missing["Items"], json!([]));
        assert_eq!(missing["TotalRecordCount"], 0);
    }
}

#[tokio::test]
async fn web_episode_queue_merges_servers_but_keeps_alternate_primary_first() {
    let (f, _, raw_ids) = secondary_show(120).await;
    f.state.config.write().await.deduplicate_media = true;
    for upstream in &f.upstreams {
        Mock::given(method("GET")).and(path("/Items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "Items": [{"Id": SERIES, "Type": "Series", "Name": "Show", "ProviderIds": {"Tvdb": "42"}}],
                "StartIndex": 0, "TotalRecordCount": 1
            }))).mount(upstream).await;
    }
    Mock::given(method("GET"))
        .and(path(format!("/Shows/{SERIES}/Episodes")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Items": [{"Id": episode(119), "Type": "Episode", "SeriesId": SERIES,
                "ParentIndexNumber": 6, "IndexNumber": 20}],
            "StartIndex": 0, "TotalRecordCount": 1
        })))
        .mount(&f.upstreams[0])
        .await;
    let shows = web_catalog(&f, "/Items?SearchTerm=Show").await;
    let series = shows["Items"][0]["Id"].as_str().unwrap();
    let listing = web_catalog(&f, &format!("/Shows/{series}/Episodes")).await;
    let episodes = listing["Items"].as_array().unwrap();
    assert_eq!(episodes.len(), 120);
    let canonical = episodes[119]["Id"].as_str().unwrap();
    let queue = web_catalog(&f, &queue_url(series, canonical)).await;
    let items = queue["Items"].as_array().unwrap();
    assert_eq!(items.len(), 100);
    assert_eq!(items[99]["Id"], canonical);
    assert_eq!(items[98]["Id"], episodes[118]["Id"]);

    let alternate = web_catalog(&f, &queue_url(series, &raw_ids[119])).await;
    assert_eq!(alternate["Items"][0]["Id"], canonical,
        "stock Web selects index zero when its alternate Id is absent; never prepend an unrelated episode");
    assert_eq!(alternate["Items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn web_episode_queue_validates_the_anchor_before_widening_the_query() {
    let (f, series, _) = secondary_show(3).await;
    let server = f
        .state
        .server_storage
        .add_server(
            "Forbidden",
            "http://127.0.0.1:1",
            1,
            MediaStreamingMode::Proxy,
        )
        .await
        .unwrap();
    let server = f
        .state
        .server_storage
        .get_server_by_id(server)
        .await
        .unwrap()
        .unwrap();
    let forbidden = f
        .state
        .media_storage
        .get_or_create_media_mapping(&episode(500), &server)
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .get(format!(
            "{}{}",
            f.url,
            queue_url(&series, &forbidden.virtual_media_id)
        ))
        .header("Authorization", web_auth(&f).to_header_value())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(f.upstreams[1]
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| !request.url.path().ends_with("/Episodes")));
}

#[tokio::test]
async fn web_episode_queue_does_not_return_a_truncated_queue_after_a_failed_page() {
    let (f, series, ids) = secondary_show(120).await;
    Mock::given(method("GET"))
        .and(path(format!("/Shows/{SERIES}/Episodes")))
        .and(query_param("StartIndex", "37"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&f.upstreams[1])
        .await;
    let response = reqwest::Client::new()
        .get(format!("{}{}", f.url, queue_url(&series, &ids[119])))
        .header("Authorization", web_auth(&f).to_header_value())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn web_episode_browsing_is_not_mistaken_for_a_playback_queue() {
    let (f, series, ids) = secondary_show(4).await;
    let result = web_catalog(
        &f,
        &format!("/Shows/{series}/Episodes?StartItemId={}&Limit=100", ids[2]),
    )
    .await;
    assert_eq!(result["Items"].as_array().unwrap().len(), 2);
    assert_eq!(result["Items"][0]["Id"], ids[2]);
    let requests = f.upstreams[1].received_requests().await.unwrap();
    assert!(requests
        .iter()
        .filter(|request| request.url.path().ends_with("/Episodes"))
        .all(|request| request
            .url
            .query_pairs()
            .any(|(key, _)| key.eq_ignore_ascii_case("StartItemId"))));
}
