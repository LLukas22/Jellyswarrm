//! No silent holes: each discovered route must have an executable contract,
//! a workflow contract, or an explicit scope decision.
use super::*;

fn route_pattern(route: &str) -> regex::Regex {
    let route = route.trim_matches('/').to_ascii_lowercase();
    let placeholders = regex::Regex::new(r"\{[^}]*\}").unwrap();
    let mut result = String::from("^");
    let mut end = 0;
    for placeholder in placeholders.find_iter(&route) {
        result.push_str(&regex::escape(&route[end..placeholder.start()]));
        result.push_str("[^/]+");
        end = placeholder.end();
    }
    result.push_str(&regex::escape(&route[end..]));
    result.push('$');
    regex::Regex::new(&result).unwrap()
}

// These are full request sequences; listing them here does not create a second
// mock of the same behavior. References are checked against the test source.
const WORKFLOWS: &[(&str, &str, &str)] = &[
    (
        "POST",
        "/Items/{id}/PlaybackInfo",
        "web_version_name_survives_playback_negotiation",
    ),
    (
        "POST",
        "/LiveStreams/Open",
        "sdk_body_only_open_translates_ids_and_query_overrides_body",
    ),
    (
        "POST",
        "/LiveStreams/MediaInfo",
        "live_actions_follow_owner_and_close_only_releases_on_success",
    ),
    (
        "POST",
        "/LiveStreams/Close",
        "live_actions_follow_owner_and_close_only_releases_on_success",
    ),
    (
        "DELETE",
        "/Videos/ActiveEncodings",
        "stop_transcoding_uses_play_session_backend",
    ),
    (
        "POST",
        "/Sessions/Playing",
        "same_server_episode_selection_survives_streams_and_all_playback_reports",
    ),
    (
        "POST",
        "/Sessions/Playing/Progress",
        "same_server_episode_selection_survives_streams_and_all_playback_reports",
    ),
    (
        "POST",
        "/Sessions/Playing/Stopped",
        "same_server_episode_selection_survives_streams_and_all_playback_reports",
    ),
];

// Explicit exclusions: backend administration and remaining media editors,
// rather than browsing, library state, queues or playback.
// Exact endpoint patterns make newly introduced client calls fail this check.
const OUTSIDE_SCOPE: &[(&str, &str, &str)] = &[
    ("DELETE", "/Items/{id}/Images", "image editor"),
    ("POST", "/Items/{id}/Images", "image editor"),
    (
        "POST",
        "/Items/{id}/Images/{type}/{index}/Index",
        "image editor",
    ),
    ("POST", "/Videos/{id}/Subtitles", "subtitle editor"),
    ("POST", "/Videos/MergeVersions", "version editor"),
    ("DELETE", "/Videos/{id}/AlternateSources", "version editor"),
    (
        "GET",
        "/LiveTv/GuideInfo",
        "guide date-range metadata; no item IDs or playback state",
    ),
    (
        "GET",
        "/LiveTv/ChannelMappingOptions",
        "tuner configuration",
    ),
    ("POST", "/LiveTv/ChannelMappings", "tuner configuration"),
    ("POST", "/LiveTv/ListingProviders", "tuner configuration"),
    ("DELETE", "/LiveTv/ListingProviders", "tuner configuration"),
    (
        "GET",
        "/LiveTv/ListingProviders/Default",
        "tuner configuration",
    ),
    (
        "GET",
        "/LiveTv/ListingProviders/Lineups",
        "tuner configuration",
    ),
    (
        "GET",
        "/LiveTv/ListingProviders/SchedulesDirect/Countries",
        "tuner configuration",
    ),
    ("POST", "/LiveTv/TunerHosts", "tuner configuration"),
    ("DELETE", "/LiveTv/TunerHosts", "tuner configuration"),
    ("GET", "/LiveTv/TunerHosts/Types", "tuner configuration"),
    ("GET", "/LiveTv/Tuners/Discover", "tuner configuration"),
];

fn contracts() -> Vec<(&'static str, &'static str)> {
    super::catalog::ROUTES
        .iter()
        .map(|path| (super::catalog::METHOD, *path))
        .chain(super::resources::ROUTES.iter().map(|path| ("GET", *path)))
        .chain(super::discovery::ROUTES.iter().map(|path| ("GET", *path)))
        .chain(super::bodies::ROUTES.iter().map(|path| ("POST", *path)))
        .chain(super::mutations::ROUTES.iter().copied())
        .chain(super::preferences::ROUTES.iter().copied())
        .chain(super::editing::ROUTES.iter().copied())
        .collect()
}

fn inventory() -> Value {
    serde_json::from_str(include_str!("client_routes.json")).unwrap()
}

#[test]
fn every_discovered_route_has_a_contract_or_explicit_scope_decision() {
    let inventory = inventory();
    assert_eq!(inventory["schema"], 1);
    let cases = contracts();
    let mut missing = std::collections::BTreeSet::new();
    let mut totals = [0; 3];
    for request in inventory["requests"].as_array().unwrap() {
        let method = request["method"].as_str().unwrap();
        let path = request["path"].as_str().unwrap();
        let pattern = route_pattern(path);
        let matches = |candidate_method: &str, candidate_path: &str| {
            (method == "CONTEXT" || method == candidate_method)
                && pattern.is_match(
                    &candidate_path
                        .split('?')
                        .next()
                        .unwrap()
                        .trim_matches('/')
                        .to_ascii_lowercase(),
                )
        };
        if cases.iter().any(|(m, p)| matches(m, p)) {
            totals[0] += 1;
        } else if WORKFLOWS.iter().any(|(m, p, _)| matches(m, p)) {
            totals[1] += 1;
        } else if OUTSIDE_SCOPE
            .iter()
            .any(|(m, p, reason)| !reason.is_empty() && matches(m, p))
        {
            totals[2] += 1;
        } else {
            missing.insert(format!(
                "{method} {path} ({}:{} {})",
                request["file"], request["line"], request["operation"]
            ));
        }
    }
    println!(
        "Client call sites: {} request contracts, {} workflow contracts, {} explicitly excluded",
        totals[0], totals[1], totals[2]
    );
    assert!(
        missing.is_empty(),
        "Unclassified client routes:\n{}",
        missing.into_iter().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn workflow_references_point_to_executable_tests() {
    let workflows = [
        include_str!("pipeline.rs"),
        include_str!("pipeline/regressions/streaming_routing.rs"),
        include_str!("pipeline/regressions/web_client.rs"),
        include_str!("pipeline/regressions/web_client/lifecycle.rs"),
    ]
    .join("\n");
    for (_, _, test) in WORKFLOWS {
        assert!(
            workflows.contains(&format!("async fn {test}(")),
            "missing workflow {test}"
        );
    }
}

#[test]
fn dynamic_dispatch_and_server_urls_have_reviewed_contract_families() {
    let inventory = inventory();
    for site in inventory["dynamic"].as_array().unwrap() {
        let file = site["file"].as_str().unwrap();
        let reason = site["reason"].as_str().unwrap();
        let covered = match reason {
            "legacy getScaledImageUrl: computed URL" => true, // resources::artwork/chapter_artwork
            "dynamic method dispatch" => [
                "ui/src/apps/legacy/controllers/list.js",
                "ui/src/components/playback/playbackmanager.js",
                "ui/src/components/userdatabuttons/userdatabuttons.js",
            ]
            .contains(&file),
            "computed or server-supplied URL" => [
                "ui/src/components/playback/playbackmanager.js",
                "ui/src/plugins/htmlVideoPlayer/plugin.js",
            ]
            .contains(&file),
            _ => false,
        };
        let excluded = [
            "legacy getUserImageUrl: computed URL",
            "legacy addMediaPath: computed URL",
            "legacy addVirtualFolder: computed URL",
            "legacy downloadRemoteImage: computed URL",
            "legacy getAvailableRemoteImages: computed URL",
            "legacy getRemoteImageProviders: computed URL",
            "legacy getVirtualFolders: computed URL",
            "legacy removeMediaPath: computed URL",
            "legacy removeVirtualFolder: computed URL",
            "legacy renameVirtualFolder: computed URL",
            "legacy updateMediaPath: computed URL",
            "legacy updateVirtualFolderOptions: computed URL",
        ]
        .contains(&reason)
            || (reason == "computed or server-supplied URL"
                && [
                    "ui/src/components/ServerContentPage.tsx",
                    "ui/src/components/subtitleeditor/subtitleeditor.js",
                ]
                .contains(&file));
        assert!(covered || excluded, "Unreviewed dynamic call site: {site}");
    }
}
