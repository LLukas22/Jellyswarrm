//! Quality-first ordering for the versions of one merged item.
//!
//! With `prefer_best_version` enabled, the versions of a title that exists on
//! several servers are ordered by what they contain instead of by server
//! priority: resolution first, then the HDR format, then bitrate. Server
//! priority only breaks ties. Clients that pre-select the first version (the
//! official Jellyfin apps) then play the best copy, and a version that needs
//! transcoding still wins when it has the higher resolution: the backend that
//! owns it transcodes as usual.
use std::cmp::Reverse;
use std::time::Duration;

use crate::{
    domain::ids::ServerId,
    protocol::models::{MediaSource, MediaStream},
};

/// Bitrates within one bucket count as equal, so two encodes that differ by a
/// few Mbit/s fall back to server priority instead of flipping between servers.
const BITRATE_BUCKET_BPS: i64 = 5_000_000;

/// Comparable quality of one media source; a greater key is the better version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct QualityKey {
    resolution: u8,
    dynamic_range: u8,
    bitrate_bucket: i64,
}

pub(crate) fn quality_key(source: &MediaSource) -> QualityKey {
    let video = source
        .media_streams
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|stream| {
            stream
                .stream_type
                .as_deref()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("Video"))
        });
    let bitrate = source
        .bitrate
        .or_else(|| video.and_then(|stream| stream.bit_rate))
        .unwrap_or(0)
        .max(0);
    QualityKey {
        resolution: video.map_or(0, resolution_class),
        dynamic_range: video.map_or(0, dynamic_range_rank),
        bitrate_bucket: bitrate / BITRATE_BUCKET_BPS,
    }
}

/// Widths and heights are both checked so cropped (scope) and pillarboxed
/// encodes land in the class of their nominal resolution.
fn resolution_class(stream: &MediaStream) -> u8 {
    let width = stream.width.unwrap_or(0);
    let height = stream.height.unwrap_or(0);
    if width >= 3200 || height >= 2000 {
        4
    } else if width >= 2400 || height >= 1300 {
        3
    } else if width >= 1700 || height >= 1000 {
        2
    } else if width >= 1100 || height >= 700 {
        1
    } else {
        0
    }
}

/// Dolby Vision with an HDR10/HDR10+ base layer is at least as good as that
/// base everywhere. Dolby Vision without a fallback layer ranks like HDR10:
/// clients without Dolby Vision support have to transcode it.
fn dynamic_range_rank(stream: &MediaStream) -> u8 {
    let range_type = stream.video_range_type.as_deref().unwrap_or("");
    match range_type.to_ascii_uppercase().as_str() {
        "DOVIWITHHDR10PLUS" | "DOVIWITHELHDR10PLUS" => 4,
        "HDR10PLUS" | "DOVIWITHHDR10" | "DOVIWITHEL" => 3,
        "HDR10" | "DOVI" => 2,
        "HLG" | "DOVIWITHHLG" => 1,
        "" => match stream.video_range.as_deref() {
            Some(range) if range.eq_ignore_ascii_case("HDR") => 2,
            _ => 0,
        },
        _ => 0,
    }
}

/// Stable sort, best version first: equal versions keep their incoming order,
/// which is server priority.
pub(crate) fn order_by_quality(sources: &mut [MediaSource]) {
    sources.sort_by_key(|source| Reverse(quality_key(source)));
}

/// Which server held the best version of a merged item, learned from the last
/// merged detail response. Playback negotiation uses it when a client starts
/// the merged item without naming a version (for example from a home-screen
/// card), so it lands on the same version the detail page pre-selects.
pub(crate) struct BestVersionHints {
    servers: moka::future::Cache<i64, ServerId>,
}

impl BestVersionHints {
    pub(crate) fn new() -> Self {
        Self {
            servers: moka::future::Cache::builder()
                .max_capacity(20_000)
                .time_to_live(Duration::from_secs(12 * 60 * 60))
                .build(),
        }
    }

    pub(crate) async fn remember(&self, group_id: i64, server_id: ServerId) {
        self.servers.insert(group_id, server_id).await;
    }

    pub(crate) async fn best_server(&self, group_id: i64) -> Option<ServerId> {
        self.servers.get(&group_id).await
    }
}

impl Default for BestVersionHints {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source(
        id: &str,
        width: i32,
        height: i32,
        range_type: Option<&str>,
        bitrate: i64,
    ) -> MediaSource {
        serde_json::from_value(json!({
            "Id": id,
            "Bitrate": bitrate,
            "MediaStreams": [{
                "Type": "Video",
                "Index": 0,
                "Width": width,
                "Height": height,
                "VideoRangeType": range_type
            }]
        }))
        .unwrap()
    }

    fn ids(sources: &[MediaSource]) -> Vec<&str> {
        sources.iter().map(|source| source.id.as_str()).collect()
    }

    #[test]
    fn higher_resolution_wins_over_server_order_and_dynamic_range() {
        let mut sources = vec![
            source(
                "primary-1080p-dv",
                1920,
                1080,
                Some("DOVIWithHDR10"),
                20_000_000,
            ),
            source("other-2160p-sdr", 3840, 2160, Some("SDR"), 15_000_000),
        ];
        order_by_quality(&mut sources);
        assert_eq!(ids(&sources), ["other-2160p-sdr", "primary-1080p-dv"]);
    }

    #[test]
    fn scope_encodes_keep_their_nominal_resolution() {
        let mut sources = vec![
            source("primary-1080p", 1920, 1080, Some("HDR10"), 30_000_000),
            source("other-2160p-scope", 3840, 1600, Some("HDR10"), 20_000_000),
        ];
        order_by_quality(&mut sources);
        assert_eq!(ids(&sources), ["other-2160p-scope", "primary-1080p"]);
    }

    #[test]
    fn same_resolution_prefers_the_better_dynamic_range() {
        let mut sources = vec![
            source("primary-hdr10", 3840, 2160, Some("HDR10"), 25_000_000),
            source(
                "other-dv-hdr10",
                3840,
                2160,
                Some("DOVIWithHDR10"),
                25_000_000,
            ),
            source("third-sdr", 3840, 2160, Some("SDR"), 40_000_000),
        ];
        order_by_quality(&mut sources);
        assert_eq!(
            ids(&sources),
            ["other-dv-hdr10", "primary-hdr10", "third-sdr"]
        );
    }

    #[test]
    fn dolby_vision_without_fallback_ranks_like_hdr10() {
        let mut sources = vec![
            source("primary-dv-only", 3840, 2160, Some("DOVI"), 25_000_000),
            source("other-hdr10", 3840, 2160, Some("HDR10"), 25_000_000),
        ];
        order_by_quality(&mut sources);
        assert_eq!(ids(&sources), ["primary-dv-only", "other-hdr10"]);
    }

    #[test]
    fn small_bitrate_differences_keep_server_priority() {
        let mut sources = vec![
            source("primary", 3840, 2160, Some("HDR10"), 21_000_000),
            source(
                "other-slightly-higher",
                3840,
                2160,
                Some("HDR10"),
                23_000_000,
            ),
            source("third-much-higher", 3840, 2160, Some("HDR10"), 31_000_000),
        ];
        order_by_quality(&mut sources);
        assert_eq!(
            ids(&sources),
            ["third-much-higher", "primary", "other-slightly-higher"]
        );
    }

    #[test]
    fn missing_stream_details_sort_last_without_reordering_each_other() {
        let mut sources: Vec<MediaSource> = vec![
            serde_json::from_value(json!({"Id": "primary-unknown"})).unwrap(),
            source("other-720p", 1280, 720, None, 4_000_000),
            serde_json::from_value(json!({"Id": "third-unknown"})).unwrap(),
        ];
        order_by_quality(&mut sources);
        assert_eq!(
            ids(&sources),
            ["other-720p", "primary-unknown", "third-unknown"]
        );
    }

    #[test]
    fn video_range_is_the_fallback_when_the_range_type_is_missing() {
        let hdr: MediaSource = serde_json::from_value(json!({
            "Id": "hdr",
            "MediaStreams": [{"Type": "Video", "Index": 0, "Width": 3840, "Height": 2160, "VideoRange": "HDR"}]
        }))
        .unwrap();
        let sdr = source("sdr", 3840, 2160, None, 0);
        assert!(quality_key(&hdr) > quality_key(&sdr));
    }

    #[tokio::test]
    async fn hints_remember_the_best_server_per_group() {
        let hints = BestVersionHints::new();
        assert_eq!(hints.best_server(7).await, None);
        hints.remember(7, ServerId::new(2)).await;
        assert_eq!(hints.best_server(7).await, Some(ServerId::new(2)));
        hints.remember(7, ServerId::new(1)).await;
        assert_eq!(hints.best_server(7).await, Some(ServerId::new(1)));
    }
}
