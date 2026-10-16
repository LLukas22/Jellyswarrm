//! Body-bearing client requests: identity fields must be translated recursively,
//! while names, track settings, permission flags and other payload data survive.
use super::*;
use wiremock::{
    matchers::{body_json, method, path, query_param},
    Mock, ResponseTemplate,
};

macro_rules! contracts {
    ($($name:ident: $route:literal, $body:expr;)+) => {
        pub(super) const ROUTES: &[&str] = &[$($route),+];
        $(scenario_test!($name, body_contract, $route, $body);)+
    };
}
contracts! {
    create_playlist: "/Playlists", json!({"Name":"My queue","Ids":["{item}"],"UserId":"{user}","MediaType":"Video","IsPublic":false});
    update_playlist: "/Playlists/{item}", json!({"Name":"Updated queue","Ids":["{item}"],"Shares":[{"UserId":"{user}","CanEdit":true}],"OpenAccess":false});
    create_collection: "/Collections?Name=My%20collection&Ids={item}", Value::Null;
    update_timer: "/LiveTv/Timers/{item}", json!({"Id":"{item}","ChannelId":"{item}","Name":"Recording","PrePaddingSeconds":60,"PostPaddingSeconds":120});
    update_series_timer: "/LiveTv/SeriesTimers/{item}", json!({"Id":"{item}","ChannelId":"{item}","Name":"Series","RecordAnyTime":true,"RecordNewOnly":true});
    posted_guide_translates_channel_id_arrays: "/LiveTv/Programs", json!({"ChannelIds":["{item}"],"UserId":"{user}","StartIndex":0,"Limit":100});
    recording_creation_translates_program_id: "/LiveTv/Timers", json!({"ProgramId":"{item}","ChannelId":"{item}","Name":"Recording"});
    series_recording_creation_translates_program_id: "/LiveTv/SeriesTimers", json!({"ProgramId":"{item}","ChannelId":"{item}","Name":"Series recording","RecordNewOnly":true});
    metadata_edit_reaches_item_owner: "/Items/{item}", json!({"Id":"{item}","Name":"Updated movie","ProviderIds":{"Tmdb":"42"},"Overview":"Keep this text"});
}

pub(super) async fn body_contract(scenario: Scenario, route: &str, body: Value) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    let original = f.original(1).await;
    let substitute =
        |text: &str, item: &str, user: &str| text.replace("{item}", item).replace("{user}", user);
    let client = substitute(route, &f.items[1], &f.user.id);
    let backend = substitute(route, &original, "backend-user-1");
    let url = url::Url::parse(&format!("http://backend{backend}")).unwrap();
    let client_body: Value =
        serde_json::from_str(&substitute(&body.to_string(), &f.items[1], &f.user.id)).unwrap();
    let backend_body: Value =
        serde_json::from_str(&substitute(&body.to_string(), &original, "backend-user-1")).unwrap();
    let mut mock = Mock::given(method("POST")).and(path(url.path()));
    for (key, value) in url.query_pairs() {
        mock = mock.and(query_param(key.as_ref(), value.as_ref()));
    }
    if !body.is_null() {
        mock = mock.and(body_json(backend_body));
    }
    let creates = ["/Playlists", "/Collections"].contains(&url.path());
    let guide = url.path() == "/LiveTv/Programs";
    let status = if creates || guide { 200 } else { 204 };
    let template = if creates {
        ResponseTemplate::new(status).set_body_json(json!({"Id":original}))
    } else if guide {
        ResponseTemplate::new(status)
            .set_body_json(json!({"Items":[],"TotalRecordCount":0,"StartIndex":0}))
    } else {
        ResponseTemplate::new(status)
    };
    mock.respond_with(template)
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let mut request = f.request(reqwest::Method::POST, &client, &f.user, "tv");
    if !body.is_null() {
        request = request.json(&client_body);
    }
    let response = request.send().await.unwrap();
    assert_eq!(
        response.status().as_u16(),
        status,
        "{route}; upstream requests: {:?}",
        f.upstreams[1].received_requests().await.unwrap()
    );
    if creates {
        assert_eq!(response.json::<Value>().await.unwrap()["Id"], f.items[1]);
    }
    let requests = f.upstreams[1].received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "{route}");
    assert!(requests[0]
        .headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("backend-token-1"));
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}
