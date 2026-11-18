//! Memory and request-count measurements for large catalogs.
//!
//! Items follow an episode captured from the development Jellyfin servers as
//! requested by Jellyfin web's Episodes view. Run alone, because allocation
//! counts are process-wide:
//!
//! ```text
//! cargo test -p jellyswarrm-proxy --lib scale_tests -- --ignored --nocapture --test-threads=1
//! ```
use super::tests::{request, setup};
use super::*;
use crate::test_support::alloc;
use serde_json::json;
use std::time::Instant;
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

const EPISODES_PER_SERVER: usize = 22_000;
const EPISODES_PER_SEASON: usize = 11;
const SEASONS_PER_SERIES: usize = 10;

fn hex_id(kind: usize, server: usize, index: usize) -> String {
    format!("{kind:08x}{server:08x}{index:016x}")
}

/// One episode shaped like the development servers' response. Series and
/// season fields repeat across episodes. Personal state depends on the viewer.
fn episode(server: usize, index: usize, second_viewer: bool) -> serde_json::Value {
    let season = index / EPISODES_PER_SEASON;
    let series = season / SEASONS_PER_SERIES;
    let number = index % EPISODES_PER_SEASON + 1;
    let id = hex_id(1, server, index);
    let series_id = hex_id(2, server, series);
    let season_id = hex_id(3, server, season);
    let primary_tag = hex_id(4, server, index);
    let series_tag = hex_id(5, server, series);
    let season_tag = hex_id(6, server, season);
    let backdrop_tag = hex_id(7, server, series);
    // A realistic viewer has watched some episodes and is in the middle of a few.
    let watched = (index + usize::from(second_viewer) * 7).is_multiple_of(10);
    let in_progress = (index + usize::from(second_viewer) * 3).is_multiple_of(33);
    let mut user_data = json!({
        "PlaybackPositionTicks": if in_progress { 600_000_000 } else { 0 },
        "PlayCount": u32::from(watched),
        "IsFavorite": false,
        "Played": watched,
        "Key": format!("{}{:03}{:03}", 100_000 + series, season % SEASONS_PER_SERIES + 1, number),
        "ItemId": id,
    });
    if watched {
        user_data["LastPlayedDate"] = json!("2026-10-01T20:15:00.0000000Z");
    }
    json!({
        "Name": format!("Episode title {index:05}"),
        "ServerId": format!("{server:032x}"),
        "Id": id,
        "HasSubtitles": true,
        "Container": "mov,mp4,m4a,3gp,3g2,mj2",
        "PremiereDate": format!("{}-01-08T00:00:00.0000000Z", 1950 + index % 70),
        "ChannelId": null,
        "RunTimeTicks": 1_200_195_330u64 + index as u64,
        "ProductionYear": 1950 + index % 70,
        "IndexNumber": number,
        "ParentIndexNumber": season % SEASONS_PER_SERIES + 1,
        "IsFolder": false,
        "Type": "Episode",
        "ParentBackdropItemId": series_id,
        "ParentBackdropImageTags": [backdrop_tag],
        "UserData": user_data,
        "SeriesName": format!("Series {series:04}"),
        "SeriesId": series_id,
        "SeasonId": season_id,
        "PrimaryImageAspectRatio": 1.3333333333333333,
        "SeriesPrimaryImageTag": series_tag,
        "SeasonName": format!("Season {}", season % SEASONS_PER_SERIES + 1),
        "VideoType": "VideoFile",
        "ImageTags": {"Primary": primary_tag},
        "BackdropImageTags": [],
        "ImageBlurHashes": {
            "Primary": {
                primary_tag: "V01Ci{RjWUWBxbkAj[t7ofWBM{j[t7j[Rjxuj[Rjaft7",
                series_tag: "dNH1oD={9a-o58E2NGt70LIpR-oz?Gj[t6oLShxZ%0ad",
                season_tag.clone(): "dNH1oD={9a-o58E2NGt70LIpR-oz?Gj[t6oLShxZ%0ad"
            },
            "Backdrop": {
                backdrop_tag: "WBC?$J_LBUISFZJ%4m9YIT%gr^MxI;%NVYIqv~xa.8RQRPM{tSIn"
            }
        },
        "ParentPrimaryImageItemId": season_id,
        "ParentPrimaryImageTag": season_tag,
        "LocationType": "FileSystem",
        "MediaType": "Video"
    })
}

fn query_value(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.into_owned())
}

fn mib(bytes: isize) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

#[tokio::test]
#[ignore = "builds two 22,000-episode catalogs and measures memory"]
async fn scale_listing_memory() {
    let (mut state, _, sessions, upstreams) = setup().await;
    // A budget large enough that every measured listing stays cached.
    state.services.catalog_snapshots = std::sync::Arc::new(snapshots::CatalogSnapshots::new(
        crate::config::CatalogCacheSettings {
            max_bytes: 8 << 30,
            ..crate::config::CatalogCacheSettings::default()
        },
    ));
    for (server, upstream) in upstreams.iter().enumerate() {
        Mock::given(method("GET"))
            .and(path("/Items"))
            .respond_with(move |request: &wiremock::Request| {
                let start: usize = query_value(request, "StartIndex")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let limit: usize = query_value(request, "Limit")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(EPISODES_PER_SERVER);
                let second_viewer = request.headers.values().any(|value| {
                    value
                        .to_str()
                        .is_ok_and(|value| value.contains("upstream-token-2"))
                });
                let items = (start..EPISODES_PER_SERVER.min(start + limit))
                    .map(|index| episode(server, index, second_viewer))
                    .collect::<Vec<_>>();
                ResponseTemplate::new(200).set_body_json(json!({
                    "Items": items,
                    "TotalRecordCount": EPISODES_PER_SERVER,
                    "StartIndex": start
                }))
            })
            .mount(upstream)
            .await;
    }
    let second_sessions = sessions
        .iter()
        .map(|(session, server)| {
            let mut session = session.clone();
            session.id += 100;
            session.user_id = "viewer-2".into();
            session.jellyfin_token = "upstream-token-2".into();
            session.original_user_id = "backend-user-2".into();
            (session, server.clone())
        })
        .collect::<Vec<_>>();
    let as_second_viewer = |mut operation: crate::application::request::input::OperationRequest| {
        operation.context = crate::application::request::context::RequestContext::new(
            &operation.original_request,
            None,
            None,
            Some(second_sessions.clone()),
            Some(crate::libraries::service::VirtualLibraryAccessScope::new(
                "viewer-2",
                second_sessions.iter().map(|(_, server)| server.id),
            )),
        );
        operation
    };

    let base = "/Items?Recursive=true&IncludeItemTypes=Episode&Fields=PrimaryImageAspectRatio,MediaSourceCount&Limit=20";
    let listings: Vec<(&str, crate::application::request::input::OperationRequest)> = vec![
        (
            "first listing",
            request(&format!("{base}&SortBy=SortName"), &sessions),
        ),
        (
            "second sort, same viewer",
            request(&format!("{base}&SortBy=PremiereDate,SortName"), &sessions),
        ),
        ("second device, same viewer", {
            let mut operation = request(&format!("{base}&SortBy=SortName"), &sessions);
            operation
                .original_request
                .headers_mut()
                .insert("User-Agent", "second-device".parse().unwrap());
            operation
        }),
        (
            "second viewer, different watched state",
            as_second_viewer(request(&format!("{base}&SortBy=SortName"), &sessions)),
        ),
    ];

    // Warm up mapping storage so that the measured builds only add listings.
    let warmup = get_catalog(
        &state.services,
        request(&format!("{base}&SortBy=Name"), &sessions),
    )
    .await
    .unwrap();
    assert_eq!(warmup["TotalRecordCount"], 2 * EPISODES_PER_SERVER);
    drop(warmup);
    state.catalog_snapshots.expire();
    tokio::task::yield_now().await;

    let start = alloc::live_bytes();
    eprintln!("baseline live heap: {}", mib(start));
    let mut previous = start;
    let mut requests_before = 0;
    for upstream in &upstreams {
        requests_before += upstream.received_requests().await.unwrap().len();
    }
    for (label, operation) in listings {
        let began = Instant::now();
        let page = get_catalog(&state.services, operation).await.unwrap();
        let elapsed = began.elapsed();
        assert_eq!(page["TotalRecordCount"], 2 * EPISODES_PER_SERVER);
        assert_eq!(page["Items"].as_array().unwrap().len(), 20);
        drop(page);
        let live = alloc::live_bytes();
        eprintln!(
            "{label:<40} retained +{:>10}  process peak so far +{:>10}  cold {:>8.0?}",
            mib(live - previous),
            mib(alloc::peak_bytes() - start),
            elapsed,
        );
        previous = live;
    }
    let mut requests_after = 0;
    for upstream in &upstreams {
        requests_after += upstream.received_requests().await.unwrap().len();
    }
    eprintln!(
        "upstream requests for 4 cold listings: {}",
        requests_after - requests_before
    );

    // Warm deep pages must be served from the retained listing.
    let began = Instant::now();
    for start in [0, 21_000, 43_980] {
        let page = get_catalog(
            &state.services,
            request(
                &format!("{base}&SortBy=SortName&StartIndex={start}"),
                &sessions,
            ),
        )
        .await
        .unwrap();
        assert_eq!(page["StartIndex"], start);
    }
    eprintln!("3 warm pages: {:?}", began.elapsed());
    let mut requests_warm = 0;
    for upstream in &upstreams {
        requests_warm += upstream.received_requests().await.unwrap().len();
    }
    assert_eq!(requests_warm, requests_after, "warm pages must not scan");

    eprintln!("total retained by 4 listings: {}", mib(previous - start));
    state.catalog_snapshots.expire();
    tokio::task::yield_now().await;
    eprintln!(
        "after dropping every listing: {} above baseline",
        mib(alloc::live_bytes() - start)
    );
}
