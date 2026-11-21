//! Gate for the catch-all: which requests without a Jellyfin token may still be
//! forwarded to a backend.
//!
//! Requests that carry a token are always forwarded, whatever the path, so odd
//! client and plugin routes keep working. Requests without a token only reach a
//! backend when their path is on a small allowlist of endpoints that Jellyfin
//! itself serves anonymously. Everything else is answered locally with a 404,
//! so unauthenticated probes (`/config.php`, `/.env`, ...) never reach an
//! upstream server and cannot get the proxy's IP banned there.
//!
//! Operators can extend the list with `JELLYSWARRM_EXTRA_PUBLIC_ROUTES`, a
//! comma-separated list of patterns in the same syntax as [`DEFAULT_PUBLIC_ROUTES`].
use crate::protocol::caller::JellyfinAuthorization;
use axum::extract::Request;
use percent_encoding::percent_decode_str;
use std::sync::LazyLock;
use tracing::{info, warn};

/// Environment variable with additional unauthenticated route patterns.
pub(crate) const EXTRA_PUBLIC_ROUTES_ENV: &str = "JELLYSWARRM_EXTRA_PUBLIC_ROUTES";

/// Pattern syntax, matched case-insensitively against path segments:
/// - a literal segment matches itself;
/// - `*` matches exactly one segment;
/// - a final `**` matches any remainder, including nothing.
///
/// Routes with a dedicated handler (`/System/Info/Public`, `/Users/Public`,
/// `/Users/AuthenticateByName`, `/QuickConnect/*`, `/Branding/Configuration`)
/// never reach the catch-all and need no entry here.
pub(crate) const DEFAULT_PUBLIC_ROUTES: &[&str] = &[
    "/System/Ping",
    "/System/Info/Public",
    "/Branding/**",
    "/QuickConnect/**",
    "/web/**",
    "/Items/*/Images/*/**",
    "/Users/*/Images/*/**",
    "/Persons/*/Images/*/**",
    "/Artists/*/Images/*/**",
    "/Genres/*/Images/*/**",
    "/MusicGenres/*/Images/*/**",
    "/Studios/*/Images/*/**",
];

static PUBLIC_ROUTES: LazyLock<Vec<RoutePattern>> = LazyLock::new(|| {
    let extra = std::env::var(EXTRA_PUBLIC_ROUTES_ENV).unwrap_or_default();
    let extra = parse_patterns(&extra);
    if !extra.is_empty() {
        info!(
            "Allowing extra unauthenticated routes from {EXTRA_PUBLIC_ROUTES_ENV}: {:?}",
            extra.iter().map(|p| &p.source).collect::<Vec<_>>()
        );
    }
    DEFAULT_PUBLIC_ROUTES
        .iter()
        .filter_map(|p| RoutePattern::parse(p))
        .chain(extra)
        .collect()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutePattern {
    source: String,
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Any,
    Rest,
}

impl RoutePattern {
    pub(crate) fn parse(pattern: &str) -> Option<Self> {
        let pattern = pattern.trim();
        if !pattern.starts_with('/') {
            return None;
        }
        let parts: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
        let mut segments = Vec::with_capacity(parts.len());
        for (index, part) in parts.iter().enumerate() {
            let segment = match *part {
                "**" if index + 1 == parts.len() => Segment::Rest,
                "**" => return None,
                "*" => Segment::Any,
                literal => Segment::Literal(literal.to_ascii_lowercase()),
            };
            segments.push(segment);
        }
        Some(Self {
            source: pattern.to_owned(),
            segments,
        })
    }

    fn matches(&self, path: &[String]) -> bool {
        let mut path = path.iter();
        for segment in &self.segments {
            match segment {
                Segment::Rest => return true,
                Segment::Any => {
                    if path.next().is_none() {
                        return false;
                    }
                }
                Segment::Literal(literal) => match path.next() {
                    Some(part) if part.eq_ignore_ascii_case(literal) => {}
                    _ => return false,
                },
            }
        }
        path.next().is_none()
    }
}

/// Parse a comma-separated pattern list; invalid entries are logged and skipped.
pub(crate) fn parse_patterns(list: &str) -> Vec<RoutePattern> {
    list.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let parsed = RoutePattern::parse(p);
            if parsed.is_none() {
                warn!("Ignoring invalid entry in {EXTRA_PUBLIC_ROUTES_ENV}: {p:?}");
            }
            parsed
        })
        .collect()
}

/// Split a request path into percent-decoded segments. Returns `None` for paths
/// that try to smuggle structure past the matcher: dot segments, empty segments
/// and segments that decode to a path separator.
fn path_segments(path: &str) -> Option<Vec<String>> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    trimmed
        .split('/')
        .map(|raw| {
            let decoded = percent_decode_str(raw).decode_utf8().ok()?;
            let invalid = decoded.is_empty()
                || decoded == "."
                || decoded == ".."
                || decoded.contains(['/', '\\']);
            (!invalid).then(|| decoded.into_owned())
        })
        .collect()
}

pub(crate) fn is_public_path(path: &str, patterns: &[RoutePattern]) -> bool {
    path_segments(path).is_some_and(|segments| patterns.iter().any(|p| p.matches(&segments)))
}

/// True when the request carries a Jellyfin access token in any supported form
/// (`Authorization`/`X-Emby-Authorization` with `Token=`, `X-Emby-Token`,
/// `X-MediaBrowser-Token`, or the `api_key`/`ApiKey` query parameter).
pub(crate) fn has_token(req: &Request) -> bool {
    let Ok(url) = super::incoming::request_url(req.uri()) else {
        return false;
    };
    JellyfinAuthorization::from_headers_url(req.headers(), &url)
        .as_ref()
        .and_then(JellyfinAuthorization::token_ref)
        .is_some_and(|token| !token.trim().is_empty())
}

/// Whether the catch-all may forward this request to a backend.
pub(crate) fn may_forward(req: &Request) -> bool {
    has_token(req) || is_public_path(req.uri().path(), &PUBLIC_ROUTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn defaults() -> Vec<RoutePattern> {
        DEFAULT_PUBLIC_ROUTES
            .iter()
            .map(|p| RoutePattern::parse(p).expect("default pattern parses"))
            .collect()
    }

    fn request(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn default_patterns_all_parse() {
        assert_eq!(defaults().len(), DEFAULT_PUBLIC_ROUTES.len());
    }

    #[test]
    fn anonymous_jellyfin_endpoints_are_public() {
        let patterns = defaults();
        for path in [
            "/System/Ping",
            "/system/ping",
            "/System/Info/Public",
            "/Branding/Css",
            "/Branding/Css.css",
            "/Branding/Splashscreen",
            "/QuickConnect/Enabled",
            "/web/index.html",
            "/web/",
            "/Items/0123456789abcdef0123456789abcdef/Images/Primary",
            "/Items/0123456789abcdef0123456789abcdef/Images/Backdrop/0",
            "/Users/0123456789abcdef0123456789abcdef/Images/Primary",
            "/Persons/Tom%20Hanks/Images/Primary",
            "/Studios/Studio%20Ghibli/Images/Thumb",
        ] {
            assert!(is_public_path(path, &patterns), "{path} should be public");
        }
    }

    #[test]
    fn probes_and_private_endpoints_are_not_public() {
        let patterns = defaults();
        for path in [
            "/",
            "/config.php",
            "/secrets.json",
            "/.env",
            "/.git/config",
            "/wp-login.php",
            "/System/Info",
            "/System/Logs",
            "/System/Configuration",
            "/Startup/Configuration",
            "/Items",
            "/Items/abc",
            "/Items/abc/Images",
            "/Users/abc/Items",
            "/Library/VirtualFolders",
            "/Plugins",
            "/Videos/abc/Trickplay/320/0.jpg",
        ] {
            assert!(
                !is_public_path(path, &patterns),
                "{path} must not be public"
            );
        }
    }

    #[test]
    fn traversal_and_encoded_separators_are_rejected() {
        let patterns = defaults();
        for path in [
            "/Branding/../config.php",
            "/Branding/%2e%2e/config.php",
            "/web/..%2F..%2Fconfig.php",
            "/web/%2F/config.php",
            "/web/%5C..%5Cconfig.php",
            "/Items//Images/Primary",
            "/web/./index.html",
            "/web/%ff",
        ] {
            assert!(!is_public_path(path, &patterns), "{path} must be rejected");
        }
    }

    #[test]
    fn pattern_parsing() {
        assert!(RoutePattern::parse("no-leading-slash").is_none());
        assert!(RoutePattern::parse("/a/**/b").is_none());
        assert!(RoutePattern::parse(" /Plugin/Thing/** ").is_some());
        assert_eq!(
            parse_patterns(" /Foo/* , bad, ,/Bar/**"),
            vec![
                RoutePattern::parse("/Foo/*").unwrap(),
                RoutePattern::parse("/Bar/**").unwrap()
            ]
        );
    }

    #[test]
    fn single_wildcard_matches_exactly_one_segment() {
        let patterns = parse_patterns("/Plugin/*/Logo");
        assert!(is_public_path("/plugin/abc/logo", &patterns));
        assert!(!is_public_path("/Plugin/Logo", &patterns));
        assert!(!is_public_path("/Plugin/a/b/Logo", &patterns));
        assert!(!is_public_path("/Plugin/abc/Logo/extra", &patterns));
    }

    #[test]
    fn extra_patterns_extend_the_defaults() {
        let mut patterns = defaults();
        assert!(!is_public_path("/Intros/Public/abc", &patterns));
        patterns.extend(parse_patterns("/Intros/Public/**"));
        assert!(is_public_path("/Intros/Public/abc", &patterns));
    }

    #[test]
    fn token_detection_covers_all_supported_forms() {
        assert!(has_token(&request(
            "/anything",
            &[(
                "Authorization",
                r#"MediaBrowser Client="x", Device="y", DeviceId="z", Version="1", Token="abc""#
            )]
        )));
        assert!(has_token(&request(
            "/anything",
            &[(
                "X-Emby-Authorization",
                r#"MediaBrowser Client="x", Device="y", DeviceId="z", Version="1", Token="abc""#
            )]
        )));
        assert!(has_token(&request("/anything", &[("X-Emby-Token", "abc")])));
        assert!(has_token(&request("/anything?api_key=abc", &[])));
    }

    #[test]
    fn requests_without_a_token_are_anonymous() {
        assert!(!has_token(&request("/config.php", &[])));
        assert!(!has_token(&request(
            "/config.php",
            &[("User-Agent", "Go-http-client/2.0")]
        )));
        // A MediaBrowser header without Token= is how clients log in; it is not a credential.
        assert!(!has_token(&request(
            "/config.php",
            &[(
                "Authorization",
                r#"MediaBrowser Client="x", Device="y", DeviceId="z", Version="1""#
            )]
        )));
    }

    #[test]
    fn may_forward_combines_token_and_allowlist() {
        assert!(!may_forward(&request("/config.php", &[])));
        assert!(may_forward(&request("/System/Ping", &[])));
        assert!(may_forward(&request(
            "/config.php",
            &[("X-Emby-Token", "abc")]
        )));
    }
}
