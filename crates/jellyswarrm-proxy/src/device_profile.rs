//! Device classification and mobile data-saver profiles.
//!
//! Jellyswarrm recognizes clients through the Jellyfin handshake headers
//! (`Authorization` / `X-Emby-Authorization`), which carry `Client`, `Device`,
//! `DeviceId` and `Version`, plus the `User-Agent` header. This module turns
//! that identity into a coarse [`DeviceClass`] and derives a data-saver
//! profile that is applied to mobile clients so that streaming over cellular
//! connections uses less bandwidth and stays stable on flaky links:
//!
//! * stream requests get a `MaxStreamingBitrate` cap (the upstream server
//!   transcodes down to it),
//! * `PlaybackInfo` requests get a `MaxStreamingBitrate` injected into the
//!   body so client and server agree before playback starts,
//! * image requests get `maxWidth`/`quality` parameters appended so posters
//!   and backdrops are served downscaled.

use crate::config::AppConfig;
use crate::user_authorization_service::{normalize_device, Device};

/// Coarse device class derived from the Jellyfin client handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceClass {
    /// Phones and tablets (Jellyfin Mobile, Swiftfin, Findroid, ...).
    Mobile,
    /// TVs and streaming sticks (Android TV, webOS, Tizen, Roku, Chromecast, ...).
    Tv,
    /// Desktop and web clients (Jellyfin Web, Jellyfin Desktop, browsers, ...).
    Desktop,
    /// Anything we could not classify.
    Unknown,
}

impl DeviceClass {
    /// Whether this class should receive the mobile data-saver profile.
    pub fn is_mobile(self) -> bool {
        matches!(self, DeviceClass::Mobile)
    }
}

/// Client name fragments that indicate a TV/streaming-stick client.
/// Checked before the mobile markers: "Jellyfin Android TV" must classify as
/// `Tv`, not `Mobile`. Bare "samsung"/"lg" are intentionally absent here —
/// they are common phone model names in the `Device` field.
const TV_CLIENT_MARKERS: &[&str] = &[
    "android tv",
    "androidtv",
    "webos",
    "tizen",
    "roku",
    "chromecast",
    "fire tv",
    "firetv",
    "smart tv",
    "smarttv",
    "samsung tv",
    "lg tv",
    "vizio",
    "apple tv",
    "tvos",
    "tv os",
];

/// Fragments that are unambiguous TV markers even in a `Device` name or
/// `User-Agent` string (e.g. "Mozilla/5.0 (SMART-TV; Linux; ...)").
const TV_DEVICE_OR_UA_MARKERS: &[&str] = &[
    "android tv",
    "androidtv",
    "webos",
    "tizen",
    "roku",
    "chromecast",
    "fire tv",
    "firetv",
    "smart tv",
    "smarttv",
    "apple tv",
    "tvos",
    "tv os",
];

/// Client name fragments that indicate a phone/tablet client.
const MOBILE_CLIENT_MARKERS: &[&str] = &[
    "mobile",
    "swiftfin",
    "findroid",
    "jellyplayer",
    "fintasoft",
    "ios",
    "iphone",
    "ipad",
    "ipod",
    "android", // after TV markers: "Jellyfin Android TV" already returned Tv
];

/// Fragments that indicate a mobile device in a `Device` name or `User-Agent`.
const MOBILE_DEVICE_OR_UA_MARKERS: &[&str] = &[
    "mobile",
    "iphone",
    "ipad",
    "ipod",
    "android",
    "windows phone",
    "opera mini",
    "blackberry",
];

/// Client name fragments for known desktop/web clients.
const DESKTOP_CLIENT_MARKERS: &[&str] = &[
    "jellyfin web",
    "jellyfin desktop",
    "jellyfin media player",
    "emby theater",
    "jellyfin kodi",
    "mozill",
    "chrome",
    "firefox",
    "safari",
    "edge",
    "opera",
];

/// Classify a device from its handshake identity.
///
/// `client`/`device` come from the parsed `Authorization` header
/// (`X-Emby-Authorization`), `user_agent` from the `User-Agent` header when
/// available. The user agent is only consulted as a fallback so that web
/// clients opened on a phone (e.g. "Jellyfin Web" in mobile Safari) are still
/// recognized as mobile.
pub fn classify_device(client: &str, device: &str, user_agent: Option<&str>) -> DeviceClass {
    let client_norm = normalize_device(client);
    let device_norm = normalize_device(device);

    if contains_any(&client_norm, TV_CLIENT_MARKERS)
        || contains_any(&device_norm, TV_DEVICE_OR_UA_MARKERS)
    {
        return DeviceClass::Tv;
    }
    if contains_any(&client_norm, MOBILE_CLIENT_MARKERS)
        || contains_any(&device_norm, MOBILE_DEVICE_OR_UA_MARKERS)
    {
        return DeviceClass::Mobile;
    }

    if let Some(user_agent) = user_agent {
        let ua = normalize_device(user_agent);
        if contains_any(&ua, TV_DEVICE_OR_UA_MARKERS) {
            return DeviceClass::Tv;
        }
        if contains_any(&ua, MOBILE_DEVICE_OR_UA_MARKERS) {
            return DeviceClass::Mobile;
        }
    }

    if contains_any(&client_norm, DESKTOP_CLIENT_MARKERS) {
        return DeviceClass::Desktop;
    }

    DeviceClass::Unknown
}

/// Classify a [`Device`] resolved during request preprocessing.
pub fn classify_device_identity(device: &Device, user_agent: Option<&str>) -> DeviceClass {
    classify_device(&device.client, &device.device, user_agent)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

/// Data-saver settings applied to mobile clients.
#[derive(Debug, Clone, Copy)]
pub struct DataSaverProfile {
    /// Cap for stream bitrates in bits per second.
    pub max_streaming_bitrate: Option<i64>,
    /// Maximum width for served images, in pixels.
    pub image_max_width: Option<u32>,
    /// Image encoding quality (0-100).
    pub image_quality: Option<u32>,
}

impl DataSaverProfile {
    /// Build the data-saver profile from the current configuration.
    ///
    /// Values of `0` in the configuration mean "leave this untouched" and map
    /// to `None`. When data saver is disabled entirely, `None` is returned.
    pub fn from_config(config: &AppConfig) -> Option<Self> {
        if !config.mobile_data_saver_enabled {
            return None;
        }
        Some(Self {
            max_streaming_bitrate: (config.mobile_max_streaming_bitrate > 0)
                .then_some(config.mobile_max_streaming_bitrate),
            image_max_width: (config.mobile_image_max_width > 0)
                .then_some(config.mobile_image_max_width),
            image_quality: (config.mobile_image_quality > 0).then_some(config.mobile_image_quality),
        })
    }

    /// Data-saver profile for a device class, if that class qualifies.
    pub fn for_device_class(class: DeviceClass, config: &AppConfig) -> Option<Self> {
        if !class.is_mobile() {
            return None;
        }
        Self::from_config(config)
    }
}

/// Clamp the streaming bitrate query parameters of an outgoing stream URL.
///
/// Jellyfin stream endpoints honor `MaxStreamingBitrate` (and the legacy
/// `maxBitrate`) query parameters. If the client already asked for a lower
/// bitrate it is left untouched; otherwise the value is capped at `cap_bps`.
pub fn apply_stream_bitrate_cap(url: &mut url::Url, cap_bps: i64) {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    let mut clamp = |key: &str| {
        if let Some((_, value)) = pairs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            if let Ok(current) = value.parse::<i64>() {
                if current > cap_bps {
                    *value = cap_bps.to_string();
                }
            }
        }
    };

    clamp("MaxStreamingBitrate");
    clamp("maxBitrate");

    if !pairs
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("MaxStreamingBitrate"))
    {
        pairs.push(("MaxStreamingBitrate".to_string(), cap_bps.to_string()));
    }

    url.query_pairs_mut().clear().extend_pairs(pairs);
}

/// Append image downscaling parameters to an image request URL.
///
/// Only applies when the client did not already pin a size: a `width` or
/// `maxWidth` parameter is respected as-is and `quality` is only added when
/// absent.
pub fn apply_image_params(url: &mut url::Url, max_width: u32, quality: u32) {
    let has_width = url
        .query_pairs()
        .any(|(k, _)| k.eq_ignore_ascii_case("width") || k.eq_ignore_ascii_case("maxWidth"));
    let has_quality = url
        .query_pairs()
        .any(|(k, _)| k.eq_ignore_ascii_case("quality"));

    {
        let mut pairs = url.query_pairs_mut();
        if !has_width {
            pairs.append_pair("maxWidth", &max_width.to_string());
        }
        if !has_quality {
            pairs.append_pair("quality", &quality.to_string());
        }
    }
}

/// Whether a request path looks like a Jellyfin image endpoint.
pub fn is_image_request_path(path: &str) -> bool {
    path.split('/')
        .any(|segment| segment.eq_ignore_ascii_case("Images"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(client: &str, device: &str, ua: Option<&str>) -> DeviceClass {
        classify_device(client, device, ua)
    }

    #[test]
    fn mobile_clients_are_detected() {
        assert_eq!(
            classify("Jellyfin Mobile", "Pixel 8", None),
            DeviceClass::Mobile
        );
        assert_eq!(
            classify("Jellyfin Android", "SM-G991B", None),
            DeviceClass::Mobile
        );
        assert_eq!(
            classify("Swiftfin iPadOS", "iPad", None),
            DeviceClass::Mobile
        );
        assert_eq!(
            classify("Swiftfin Android", "Samsung Galaxy Tab S9", None),
            DeviceClass::Mobile
        );
        assert_eq!(classify("Findroid", "Findroid", None), DeviceClass::Mobile);
        assert_eq!(
            classify("JellyPlayer", "JellyPlayer", None),
            DeviceClass::Mobile
        );
        assert_eq!(
            classify("Jellyfin iOS", "iPhone 15", None),
            DeviceClass::Mobile
        );
    }

    #[test]
    fn tv_clients_are_not_mobile() {
        assert_eq!(
            classify("Jellyfin Android TV", "NVIDIA Shield", None),
            DeviceClass::Tv
        );
        assert_eq!(classify("Jellyfin WebOS", "LG TV", None), DeviceClass::Tv);
        assert_eq!(
            classify("Jellyfin Tizen", "Samsung TV", None),
            DeviceClass::Tv
        );
        assert_eq!(
            classify("Jellyfin Chromecast", "Living Room TV", None),
            DeviceClass::Tv
        );
        assert_eq!(
            classify("Jellyfin Roku", "Roku Streaming Stick", None),
            DeviceClass::Tv
        );
        assert_eq!(
            classify("Jellyfin Apple TV", "Apple TV", None),
            DeviceClass::Tv
        );
    }

    #[test]
    fn web_clients_classify_by_user_agent() {
        // Desktop browser without mobile UA markers.
        assert_eq!(
            classify(
                "Jellyfin Web",
                "Firefox",
                Some("Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0")
            ),
            DeviceClass::Desktop
        );
        // Same web client on a phone UA is mobile.
        assert_eq!(
            classify(
                "Jellyfin Web",
                "Mobile Safari",
                Some("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15")
            ),
            DeviceClass::Mobile
        );
        assert_eq!(
            classify(
                "Jellyfin Web",
                "Chrome",
                Some("Mozilla/5.0 (Linux; Android 13; SM-G991B) AppleWebKit/537.36")
            ),
            DeviceClass::Mobile
        );
    }

    #[test]
    fn unknown_and_desktop_fallbacks() {
        assert_eq!(
            classify("SomeUnknownClient", "Device", None),
            DeviceClass::Unknown
        );
        assert_eq!(
            classify("Jellyfin Desktop", "PC", None),
            DeviceClass::Desktop
        );
        assert_eq!(classify("Emby Theater", "PC", None), DeviceClass::Desktop);
    }

    #[test]
    fn android_tv_wins_over_android_marker() {
        // "android tv" is checked before the bare "android" mobile marker.
        assert_eq!(
            classify(
                "Jellyfin Android TV",
                "Shield",
                Some("Mozilla/5.0 (Android TV)")
            ),
            DeviceClass::Tv
        );
    }

    #[test]
    fn stream_bitrate_cap_sets_when_absent() {
        let mut url =
            url::Url::parse("http://jellyfin:8096/Videos/abc/stream.mp4?Static=true").unwrap();
        apply_stream_bitrate_cap(&mut url, 4_000_000);
        assert_eq!(
            url.query_pairs()
                .find_map(|(k, v)| { (k == "MaxStreamingBitrate").then_some(v.into_owned()) }),
            Some("4000000".to_string())
        );
    }

    #[test]
    fn stream_bitrate_cap_clamps_higher_requests() {
        let mut url =
            url::Url::parse("http://jellyfin:8096/Videos/abc/stream?MaxStreamingBitrate=120000000")
                .unwrap();
        apply_stream_bitrate_cap(&mut url, 4_000_000);
        assert_eq!(
            url.query_pairs()
                .find_map(|(k, v)| { (k == "MaxStreamingBitrate").then_some(v.into_owned()) }),
            Some("4000000".to_string())
        );
    }

    #[test]
    fn stream_bitrate_cap_respects_lower_requests() {
        let mut url =
            url::Url::parse("http://jellyfin:8096/Videos/abc/stream?maxBitrate=1500000").unwrap();
        apply_stream_bitrate_cap(&mut url, 4_000_000);
        assert_eq!(
            url.query_pairs().find_map(|(k, v)| {
                (k.eq_ignore_ascii_case("maxBitrate")).then_some(v.into_owned())
            }),
            Some("1500000".to_string())
        );
    }

    #[test]
    fn image_params_appended_when_missing() {
        let mut url =
            url::Url::parse("http://jellyfin:8096/Items/abc/Images/Primary?tag=xyz").unwrap();
        apply_image_params(&mut url, 640, 70);
        assert_eq!(
            url.query_pairs()
                .find_map(|(k, v)| { (k == "maxWidth").then_some(v.into_owned()) }),
            Some("640".to_string())
        );
        assert_eq!(
            url.query_pairs()
                .find_map(|(k, v)| { (k == "quality").then_some(v.into_owned()) }),
            Some("70".to_string())
        );
    }

    #[test]
    fn image_params_respect_client_sizes() {
        let mut url = url::Url::parse(
            "http://jellyfin:8096/Items/abc/Images/Backdrop/0?width=1920&quality=90",
        )
        .unwrap();
        apply_image_params(&mut url, 640, 70);
        assert_eq!(
            url.query_pairs()
                .find_map(|(k, v)| { (k == "width").then_some(v.into_owned()) }),
            Some("1920".to_string())
        );
        assert!(!url.query_pairs().any(|(k, _)| k == "maxWidth"));
        assert_eq!(
            url.query_pairs()
                .find_map(|(k, v)| { (k == "quality").then_some(v.into_owned()) }),
            Some("90".to_string())
        );
    }

    #[test]
    fn image_path_detection() {
        assert!(is_image_request_path("/Items/abc/Images/Primary"));
        assert!(is_image_request_path(
            "/Users/u1/Items/abc/Images/Backdrop/0"
        ));
        assert!(is_image_request_path("/Items/abc/images/logo"));
        assert!(!is_image_request_path("/Items/abc/PlaybackInfo"));
        assert!(!is_image_request_path("/Videos/abc/stream.mp4"));
    }

    #[test]
    fn data_saver_profile_disabled_by_config() {
        let config = AppConfig {
            mobile_data_saver_enabled: false,
            ..AppConfig::default()
        };
        assert!(DataSaverProfile::from_config(&config).is_none());
        assert!(DataSaverProfile::for_device_class(DeviceClass::Mobile, &config).is_none());
    }

    #[test]
    fn data_saver_profile_skips_non_mobile() {
        let config = AppConfig::default();
        assert!(DataSaverProfile::for_device_class(DeviceClass::Tv, &config).is_none());
        assert!(DataSaverProfile::for_device_class(DeviceClass::Desktop, &config).is_none());
        assert!(DataSaverProfile::for_device_class(DeviceClass::Unknown, &config).is_none());
        let profile = DataSaverProfile::for_device_class(DeviceClass::Mobile, &config).unwrap();
        assert_eq!(profile.max_streaming_bitrate, Some(4_000_000));
    }
}
