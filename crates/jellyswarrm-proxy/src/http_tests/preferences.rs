//! userSettings.js uses a client-owned key, not a backend item identity.
use super::*;
use wiremock::{
    matchers::{body_json, method, path, query_param},
    Mock, ResponseTemplate,
};

pub(super) const ROUTES: &[(&str, &str)] = &[
    ("GET", "/DisplayPreferences/usersettings"),
    ("POST", "/DisplayPreferences/usersettings"),
];

#[tokio::test]
async fn user_settings_read_write_preserves_preferences_and_translates_user() {
    let f = Fixture::new().await;
    f.healthy().await;
    let prefs = json!({"CustomPrefs":{"subtitleMode":"Smart","enableNextVideoInfoOverlay":"true"},"SortBy":"SortName","SortOrder":"Ascending"});
    Mock::given(method("GET"))
        .and(path("/DisplayPreferences/usersettings"))
        .and(query_param("UserId", "backend-user-0"))
        .and(query_param("Client", "emby"))
        .respond_with(ResponseTemplate::new(200).set_body_json(prefs.clone()))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    Mock::given(method("POST"))
        .and(path("/DisplayPreferences/usersettings"))
        .and(query_param("UserId", "backend-user-0"))
        .and(query_param("Client", "emby"))
        .and(body_json(prefs.clone()))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&f.upstreams[0])
        .await;
    let endpoint = format!(
        "/DisplayPreferences/usersettings?UserId={}&Client=emby",
        f.user.id
    );
    let response = f
        .request(reqwest::Method::GET, &endpoint, &f.user, "tv")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap(), prefs);
    let response = f
        .request(reqwest::Method::POST, &endpoint, &f.user, "tv")
        .json(&prefs)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(f.upstreams[1].received_requests().await.unwrap().is_empty());
}
