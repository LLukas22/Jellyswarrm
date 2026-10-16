//! Client item actions and container mutations, with exact request-body and
//! query translation. Aggregate/failure semantics live in pipeline regressions.
use super::*;
use wiremock::{
    matchers::{method, path, query_param},
    Mock, ResponseTemplate,
};

macro_rules! contracts {
    ($($name:ident: $verb:literal, $route:literal;)+) => {
        pub(super) const ROUTES: &[(&str,&str)] = &[$(($verb,$route)),+];
        $(scenario_test!($name, mutation_contract, $verb, $route);)+
    };
}
contracts! {
    played: "POST", "/Users/{user}/PlayedItems/{item}?DatePlayed=20261011120000";
    unplayed: "DELETE", "/Users/{user}/PlayedItems/{item}";
    sdk_played: "POST", "/UserPlayedItems/{item}?UserId={user}";
    sdk_unplayed: "DELETE", "/UserPlayedItems/{item}?UserId={user}";
    favorite: "POST", "/Users/{user}/FavoriteItems/{item}";
    unfavorite: "DELETE", "/Users/{user}/FavoriteItems/{item}";
    sdk_favorite: "POST", "/UserFavoriteItems/{item}?UserId={user}";
    sdk_unfavorite: "DELETE", "/UserFavoriteItems/{item}?UserId={user}";
    playlist_add: "POST", "/Playlists/{item}/Items?Ids={item}&UserId={user}";
    playlist_remove: "DELETE", "/Playlists/{item}/Items?EntryIds={item}";
    playlist_move: "POST", "/Playlists/{item}/Items/{item}/Move/0";
    collection_add: "POST", "/Collections/{item}/Items?Ids={item}";
    collection_remove: "DELETE", "/Collections/{item}/Items?Ids={item}";
    cancel_timer: "DELETE", "/LiveTv/Timers/{item}";
    cancel_series_timer: "DELETE", "/LiveTv/SeriesTimers/{item}";
}
async fn mutation_contract(scenario: Scenario, verb: &str, route: &str) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    let original = f.original(1).await;
    let backend = route
        .replace("{item}", &original)
        .replace("{user}", "backend-user-1");
    let url = url::Url::parse(&format!("http://backend{backend}")).unwrap();
    let mut mock = Mock::given(method(verb)).and(path(url.path()));
    for (key, value) in url.query_pairs() {
        mock = mock.and(query_param(key.as_ref(), value.as_ref()));
    }
    mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({"ItemId":original,"Played":verb=="POST","IsFavorite":verb=="POST","PlaybackPositionTicks":0,"PlayCount":0,"Key":"item"})))
        .expect(1).mount(&f.upstreams[1]).await;
    let route = route
        .replace("{item}", &f.items[1])
        .replace("{user}", &f.user.id);
    let response = f
        .request(verb.parse().unwrap(), &route, &f.user, "tv")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{verb} {route}");
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["ItemId"], f.items[1]);
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
    let requests = f.upstreams[1].received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("backend-token-1"));
}
