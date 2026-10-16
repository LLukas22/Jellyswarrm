//! POST guide federation and recording identities beyond the isolated contracts.
use super::*;
use wiremock::{
    matchers::{body_json, method, path},
    Mock, ResponseTemplate,
};

scenario_test!(
    posted_multi_channel_guide_partitions_ids_and_paginates_after_merging,
    partitioned_guide_contract
);
async fn partitioned_guide_contract(scenario: Scenario) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    for owner in 0..2 {
        let original = f.original(owner).await;
        Mock::given(method("POST")).and(path("/LiveTv/Programs"))
            .and(body_json(json!({"ChannelIds":[original],"UserId":format!("backend-user-{owner}"),"StartIndex":0,"Limit":100,"EnableImages":true})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Items":[{"Id":original,"Name":format!("Program {owner}"),"Type":"Program","ChannelId":original}],"TotalRecordCount":1,"StartIndex":0})))
            .expect(1).mount(&f.upstreams[owner]).await;
    }
    let response = f.request(reqwest::Method::POST,"/LiveTv/Programs",&f.user,"tv")
        .json(&json!({"ChannelIds":f.items,"UserId":f.user.id,"StartIndex":1,"Limit":1,"EnableImages":true})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["TotalRecordCount"], 2);
    assert_eq!(result["StartIndex"], 1);
    assert_eq!(result["Items"].as_array().unwrap().len(), 1);
    assert_eq!(result["Items"][0]["Id"], f.items[1]);
    assert_eq!(result["Items"][0]["ChannelId"], f.items[1]);
    assert_backend_tokens(&f).await;
}

scenario_test!(
    posted_unscoped_guide_advances_upstream_body_pages_without_losing_filters,
    paged_guide_contract
);
async fn paged_guide_contract(scenario: Scenario) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    for owner in 0..2 {
        for page in 0..2 {
            let id = if page == 0 {
                f.original(owner).await
            } else {
                uuid::Uuid::new_v4().simple().to_string()
            };
            Mock::given(method("POST")).and(path("/LiveTv/Programs"))
                .and(body_json(json!({"UserId":format!("backend-user-{owner}"),"StartIndex":page,"Limit":100,"MinStartDate":"2026-10-11T00:00:00Z","IsMovie":true})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Items":[{"Id":id,"Name":format!("Program {owner}-{page}"),"Type":"Program"}],"TotalRecordCount":2,"StartIndex":page})))
                .expect(1).mount(&f.upstreams[owner]).await;
        }
    }
    let response=f.request(reqwest::Method::POST,"/LiveTv/Programs",&f.user,"tv")
        .json(&json!({"UserId":f.user.id,"StartIndex":1,"Limit":2,"MinStartDate":"2026-10-11T00:00:00Z","IsMovie":true})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["TotalRecordCount"], 4);
    assert_eq!(result["StartIndex"], 1);
    assert_eq!(result["Items"].as_array().unwrap().len(), 2);
    for upstream in &f.upstreams {
        assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    }
    assert_backend_tokens(&f).await;
}

scenario_test!(
    posted_guide_body_scope_overrides_conflicting_query_scope,
    guide_scope_contract
);
async fn guide_scope_contract(scenario: Scenario) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    let original = f.original(1).await;
    Mock::given(method("POST"))
        .and(path("/LiveTv/Programs"))
        .and(body_json(
            json!({"ChannelIds":[original],"UserId":"backend-user-1"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"Items":[],"TotalRecordCount":0,"StartIndex":0})),
        )
        .expect(1)
        .mount(&f.upstreams[1])
        .await;
    let response = f
        .request(
            reqwest::Method::POST,
            &format!("/LiveTv/Programs?ChannelIds={}", f.items[0]),
            &f.user,
            "tv",
        )
        .json(&json!({"ChannelIds":[f.items[1]],"UserId":f.user.id}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
}

scenario_test!(
    posted_guide_null_pagination_is_replaced_with_numeric_upstream_pages,
    null_guide_contract
);
async fn null_guide_contract(scenario: Scenario) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    for upstream in &f.upstreams {
        Mock::given(method("POST"))
            .and(path("/LiveTv/Programs"))
            .and(body_json(
                json!({"ChannelIds":null,"StartIndex":0,"Limit":100,"IsMovie":null}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"Items":[],"TotalRecordCount":0,"StartIndex":0})),
            )
            .expect(1)
            .mount(upstream)
            .await;
    }
    let response = f
        .request(reqwest::Method::POST, "/LiveTv/Programs", &f.user, "tv")
        .json(&json!({"ChannelIds":null,"StartIndex":null,"Limit":null,"IsMovie":null}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["TotalRecordCount"], 0);
    assert_backend_tokens(&f).await;
}

scenario_test!(
    posted_guide_rejects_malformed_bodies_and_foreign_channel_owners,
    invalid_guide_contract
);
async fn invalid_guide_contract(scenario: Scenario) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    for body in [json!([]), json!({"ChannelIds":[{}]}), json!({"UserId":{}})] {
        let response = f
            .request(reqwest::Method::POST, "/LiveTv/Programs", &f.user, "tv")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = f
        .request(reqwest::Method::POST, "/LiveTv/Programs", &f.other, "tv")
        .json(&json!({"ChannelIds":[f.items[1]],"UserId":f.other.id}))
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    f.no_upstream_requests().await;
}

scenario_test!(
    recording_creation_with_only_program_id_uses_program_backend_and_preserves_settings,
    program_recording_contract
);
async fn program_recording_contract(scenario: Scenario) {
    for route in ["/LiveTv/Timers", "/LiveTv/SeriesTimers"] {
        super::bodies::body_contract(scenario,route,json!({"ProgramId":"{item}","Name":"Program only","PrePaddingSeconds":60,"PostPaddingSeconds":120,"RecordAnyChannel":false})).await;
    }
}

scenario_test!(
    recording_defaults_virtualize_program_and_channel_ids,
    recording_defaults_contract
);
async fn recording_defaults_contract(scenario: Scenario) {
    let f = Fixture::for_scenario(scenario).await;
    f.healthy().await;
    let original = f.original(1).await;
    Mock::given(method("GET")).and(path("/LiveTv/Timers/Defaults"))
        .and(wiremock::matchers::query_param("programid",&original))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ProgramId":original,"ChannelId":original,"Name":"Recording","PrePaddingSeconds":60,"PostPaddingSeconds":120})))
        .expect(1).mount(&f.upstreams[1]).await;
    let response = f
        .request(
            reqwest::Method::GET,
            &format!("/LiveTv/Timers/Defaults?programid={}", f.items[1]),
            &f.user,
            "tv",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["ProgramId"], f.items[1]);
    assert_eq!(result["ChannelId"], f.items[1]);
    assert_eq!(result["PrePaddingSeconds"], 60);
    assert_eq!(result["PostPaddingSeconds"], 120);
    assert!(f.upstreams[0].received_requests().await.unwrap().is_empty());
    assert_backend_tokens(&f).await;
}

async fn assert_backend_tokens(f: &Fixture) {
    for owner in 0..2 {
        for request in f.upstreams[owner].received_requests().await.unwrap() {
            assert!(request
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .contains(&format!("backend-token-{owner}")));
            assert!(!request.url.to_string().contains(&f.user.virtual_key));
        }
    }
}
