//! Desired contracts that expose unresolved production behavior. They remain
//! executable with `cargo test ... http_tests::known_gaps -- --ignored`.
//! Never change these to assert the currently broken response.
#[tokio::test]
#[ignore = "Videos wildcard streams AdditionalParts JSON without virtualizing returned IDs"]
async fn additional_parts_returns_client_ids() {
    super::catalog::read_contract(
        "/Videos/{item}/AdditionalParts?UserId={user}",
        super::catalog::Shape::List,
    )
    .await;
}

#[tokio::test]
#[ignore = "LiveTv/Programs is registered GET-only, rejecting the legacy client's POST form with 405"]
async fn posted_guide_translates_channel_id_arrays() {
    super::bodies::body_contract(
        "/LiveTv/Programs",
        serde_json::json!({"ChannelIds":["{item}"],"UserId":"{user}","StartIndex":0,"Limit":100}),
    )
    .await;
}

#[tokio::test]
#[ignore = "ProgramId in recording creation JSON is not translated to the backend ID"]
async fn recording_creation_translates_program_id() {
    super::bodies::body_contract(
        "/LiveTv/Timers",
        serde_json::json!({"ProgramId":"{item}","ChannelId":"{item}","Name":"Recording"}),
    )
    .await;
}

#[tokio::test]
#[ignore = "ProgramId in series recording creation JSON is not translated to the backend ID"]
async fn series_recording_creation_translates_program_id() {
    super::bodies::body_contract("/LiveTv/SeriesTimers", serde_json::json!({"ProgramId":"{item}","ChannelId":"{item}","Name":"Series recording","RecordNewOnly":true})).await;
}

#[tokio::test]
#[ignore = "ProgramId query is not used for routing or translated for recording defaults"]
async fn recording_defaults_follow_program_owner() {
    super::catalog::read_contract(
        "/LiveTv/Timers/Defaults?ProgramId={item}",
        super::catalog::Shape::Detail,
    )
    .await;
}

#[tokio::test]
#[ignore = "Items/{id} method router rejects the client's metadata POST with 405"]
async fn metadata_edit_reaches_item_owner() {
    super::bodies::body_contract("/Items/{item}",serde_json::json!({"Id":"{item}","Name":"Updated movie","ProviderIds":{"Tmdb":"42"},"Overview":"Keep this text"})).await;
}

#[tokio::test]
#[ignore = "Item ID after Items/RemoteSearch/Apply is neither routed nor translated"]
async fn remote_metadata_apply_reaches_item_owner() {
    super::editing::editing_contract(
        "POST",
        "/Items/RemoteSearch/Apply/{item}?ReplaceAllImages=true",
        serde_json::json!({"Name":"Movie","ProviderIds":{"Tmdb":"42"}}),
    )
    .await;
}
