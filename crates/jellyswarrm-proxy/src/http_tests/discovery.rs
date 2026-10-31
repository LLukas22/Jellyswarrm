//! Fan-out contracts exercise global requests independently of source-owned reads.
use super::*;
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

macro_rules! contracts {
    (
        configuration_dependent { $($scenario_name:ident: $scenario_route:literal;)+ }
        fanout { $($name:ident: $route:literal;)+ }
    ) => {
        pub(super) const ROUTES: &[&str] = &[$($scenario_route,)+ $($route),+];
        $(scenario_test!($scenario_name, discovery_contract, $scenario_route);)+
        $(contract_test!($name, discovery_contract, Scenario::NormalServers, $route);)+
    };
}
contracts! {
    configuration_dependent {
        modern_views: "/UserViews?UserId={user}";
        legacy_views: "/Users/{user}/Views";
        library_folders: "/Library/MediaFolders";
        search: "/Items?UserId={user}&SearchTerm=Movie&IncludeItemTypes=Movie";
    }
    fanout {
        movies: "/Items?UserId={user}&IncludeItemTypes=Movie&Recursive=true";
        channels: "/LiveTv/Channels?UserId={user}";
        programs: "/LiveTv/Programs?UserId={user}";
        recommendations: "/LiveTv/Programs/Recommended?UserId={user}";
        recordings: "/LiveTv/Recordings?UserId={user}";
        folders: "/LiveTv/Recordings/Folders?UserId={user}";
        persons: "/Persons?UserId={user}&SearchTerm=Actor";
        artists: "/Artists?UserId={user}&SearchTerm=Artist";
        genres: "/Genres?UserId={user}";
        studios: "/Studios?UserId={user}";
    }
}
async fn discovery_contract(scenario: Scenario, route: &str) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    let library_root = route.contains("Views") || route == "/Library/MediaFolders";
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let original = f.original(index).await;
        let target = if route == "/Library/MediaFolders" {
            format!("/Users/backend-user-{index}/Views")
        } else {
            route
                .replace("{user}", &format!("backend-user-{index}"))
                .split('?')
                .next()
                .unwrap()
                .to_string()
        };
        let kind = if target.ends_with("Views") {
            "CollectionFolder"
        } else {
            "Movie"
        };
        let name = if library_root {
            "Movies".to_string()
        } else {
            format!("Source {index}")
        };
        Mock::given(method("GET")).and(path(target))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Items":[{"Id":original,"Name":name,"Type":kind,"CollectionType":"movies"}],"TotalRecordCount":1,"StartIndex":0})))
            .expect(1).mount(upstream).await;
    }
    let response = f
        .request(
            reqwest::Method::GET,
            &route.replace("{user}", &f.user.id),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{route}");
    let result: Value = response.json().await.unwrap();
    if library_root && scenario.merged() {
        assert_eq!(result["TotalRecordCount"], 1, "{route}: {result}");
        let library = &result["Items"][0];
        assert_eq!(library["Type"], "CollectionFolder");
        assert_eq!(library["CollectionType"], "movies");
        assert!(!f
            .items
            .contains(&library["Id"].as_str().unwrap().to_string()));
    } else {
        assert_eq!(result["TotalRecordCount"], 2, "{route}: {result}");
        for id in &f.items {
            assert!(
                result["Items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["Id"] == *id),
                "{route}: {result}"
            );
        }
    }
    for (index, upstream) in f.upstreams.iter().enumerate() {
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .contains(&format!("backend-token-{index}")));
    }
}
