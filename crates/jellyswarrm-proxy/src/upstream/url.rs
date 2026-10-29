//! Shared URL helpers for ID lookup/replacement and backend URL composition.
use url::Url;
use uuid::Uuid;

pub(crate) fn is_id_like(segment: &str) -> bool {
    Uuid::parse_str(segment).is_ok()
}

/// Joins a server URL with a request path, preserving any subdirectories in the server URL
///
/// For example, `http://server.com/jellyfin` plus `/Users/123` becomes
/// `http://server.com/jellyfin/Users/123`. The module's unit tests cover path
/// composition; this helper is internal to the library.
pub(crate) fn join_server_url(server_url: &Url, request_path: &str) -> Url {
    let mut new_url = server_url.clone();
    let server_path = new_url.path().trim_end_matches('/');
    let combined_path = if server_path.is_empty() {
        request_path.to_string()
    } else {
        format!("{}{}", server_path, request_path)
    };
    new_url.set_path(&combined_path);
    new_url
}

pub(crate) fn decoded_path_segments(path: &str) -> Vec<String> {
    let decoded = percent_encoding::percent_decode_str(path).decode_utf8_lossy();
    decoded
        .strip_prefix('/')
        .unwrap_or(&decoded)
        .split('/')
        .map(str::to_owned)
        .collect()
}

pub(crate) fn contains_id(url: &Url, name: &str) -> Option<String> {
    let tags = name.split('/').collect::<Vec<_>>();
    decoded_path_segments(url.path())
        .windows(tags.len() + 1)
        .find(|parts| {
            parts
                .iter()
                .zip(&tags)
                .all(|(part, tag)| part.eq_ignore_ascii_case(tag))
                && is_id_like(&parts[tags.len()])
        })
        .map(|parts| parts[tags.len()].clone())
}

pub(crate) fn replace_id(url: Url, original: &str, replacement: &str) -> Url {
    let mut url = url;
    let segments = decoded_path_segments(url.path());
    if let Ok(mut path) = url.path_segments_mut() {
        path.clear().extend(segments.iter().map(|segment| {
            if segment == original {
                replacement
            } else {
                segment.as_str()
            }
        }));
    }
    url
}

/// Replaces the media ID immediately following `path_tag` without requiring
/// callers to know the request's full route shape.
pub(crate) fn replace_path_id(url: &Url, path_tag: &str, replacement: &str) -> Option<Url> {
    let original = contains_id(url, path_tag)?;
    Some(replace_id(url.clone(), &original, replacement))
}

/// Consolidates repeated/comma-separated values into one query parameter,
/// ensuring the requested value is present and preserving unrelated parameters.
pub(crate) fn ensure_query_list_value(url: &mut Url, expected_key: &str, value: &str) {
    let mut pairs = url
        .query_pairs()
        .map(|(key, entry)| (key.into_owned(), entry.into_owned()))
        .collect::<Vec<_>>();

    let mut values = Vec::<String>::new();
    for entry in pairs
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(expected_key))
        .flat_map(|(_, entries)| entries.split(','))
        .chain(std::iter::once(value))
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        if !values
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(entry))
        {
            values.push(entry.to_string());
        }
    }
    // Jellyfin only splits comma-delimited collections when there is one value.
    pairs.retain(|(key, _)| !key.eq_ignore_ascii_case(expected_key));
    pairs.push((expected_key.to_string(), values.join(",")));
    url.query_pairs_mut().clear().extend_pairs(pairs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_id_lookup_and_replacement_preserve_other_path_encoding() {
        let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let url = Url::parse(&format!(
            "http://localhost/%50laylists/%61{}/%55sers/name%252Fvalue/?keep=%2F",
            &id[1..]
        ))
        .unwrap();
        assert_eq!(contains_id(&url, "Playlists"), Some(id.into()));
        let rewritten = replace_id(url, id, "backend");
        assert_eq!(rewritten.path(), "/Playlists/backend/Users/name%252Fvalue/");
        assert_eq!(rewritten.query(), Some("keep=%2F"));
    }

    #[test]
    fn test_join_server_url() {
        // Test with server having subdirectory
        let server_url = Url::parse("http://server.com/jellyfin").unwrap();
        let result = join_server_url(&server_url, "/Users/123");
        assert_eq!(result.as_str(), "http://server.com/jellyfin/Users/123");

        // Test with server at root
        let server_url = Url::parse("http://server.com").unwrap();
        let result = join_server_url(&server_url, "/Users/123");
        assert_eq!(result.as_str(), "http://server.com/Users/123");

        // Test with server having trailing slash
        let server_url = Url::parse("http://server.com/jellyfin/").unwrap();
        let result = join_server_url(&server_url, "/Users/123");
        assert_eq!(result.as_str(), "http://server.com/jellyfin/Users/123");
    }

    #[test]
    fn test_is_id_like() {
        assert!(is_id_like("0123456789abcdef0123456789abcdef"));
        assert!(is_id_like("c3256b7a-96f3-4772-b7d5-cacb090bbb02")); // with dashes
        assert!(!is_id_like("0123456789abcdef0123456789abcde")); // 31 chars
        assert!(!is_id_like("g123456789abcdef0123456789abcdef")); // non-hex
    }

    #[test]
    fn test_contains_id_found() {
        let url =
            Url::parse("https://example.com/foo/0123456789abcdef0123456789abcdef/bar").unwrap();
        assert_eq!(
            contains_id(&url, "foo"),
            Some("0123456789abcdef0123456789abcdef".to_string())
        );
    }

    #[test]
    fn test_contains_id_not_found() {
        let url = Url::parse("https://example.com/foo/bar").unwrap();
        assert_eq!(contains_id(&url, "foo"), None);
    }

    #[test]
    fn deep_path_id_requires_the_complete_tag_sequence() {
        let id = "0123456789abcdef0123456789abcdef";
        let url = Url::parse(&format!(
            "https://example.com/items/remotesearch/%41pply/{id}?ReplaceAllImages=true"
        ))
        .unwrap();
        assert_eq!(
            contains_id(&url, "Items/RemoteSearch/Apply"),
            Some(id.into())
        );
        assert_eq!(contains_id(&url, "Items"), None);
        assert_eq!(contains_id(&url, "Other/RemoteSearch/Apply"), None);
        let replaced = replace_path_id(&url, "Items/RemoteSearch/Apply", "backend").unwrap();
        assert_eq!(replaced.path(), "/items/remotesearch/Apply/backend");
        assert_eq!(replaced.query(), Some("ReplaceAllImages=true"));
    }

    #[test]
    fn test_replace_id() {
        let url =
            Url::parse("https://example.com/foo/0123456789abcdef0123456789abcdef/bar").unwrap();
        let replaced = replace_id(
            url,
            "0123456789abcdef0123456789abcdef",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        assert_eq!(replaced.path(), "/foo/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/bar");
    }

    #[test]
    fn replaces_id_after_named_path_segment() {
        let url = Url::parse(
            "https://example.com/Users/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/Items/0123456789abcdef0123456789abcdef",
        )
        .unwrap();

        let replaced = replace_path_id(&url, "Items", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();

        assert_eq!(
            replaced.path(),
            "/Users/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/Items/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn query_list_consolidates_sdk_repeated_fields_without_losing_values() {
        let mut url = Url::parse("https://example.com/Items/Latest?fields=PrimaryImageAspectRatio&fields=Path&FIELDS=path,%20ProviderIds,,&enableImageTypes=Primary&enableImageTypes=Backdrop&limit=16").unwrap();
        ensure_query_list_value(&mut url, "Fields", "DateCreated");
        ensure_query_list_value(&mut url, "Fields", "ProviderIds");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![
                ("enableImageTypes".into(), "Primary".into()),
                ("enableImageTypes".into(), "Backdrop".into()),
                ("limit".into(), "16".into()),
                (
                    "Fields".into(),
                    "PrimaryImageAspectRatio,Path,ProviderIds,DateCreated".into()
                ),
            ]
        );
        let normalized = url.clone();
        ensure_query_list_value(&mut url, "Fields", "providerids");
        assert_eq!(url, normalized);
    }

    #[test]
    fn query_list_value_is_added_once() {
        let mut url = Url::parse("https://example.com/Items/id?Fields=Overview").unwrap();

        ensure_query_list_value(&mut url, "Fields", "MediaSources");
        ensure_query_list_value(&mut url, "fields", "mediasources");

        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("Fields"))
                .map(|(_, value)| value.into_owned()),
            Some("Overview,MediaSources".to_string())
        );
    }
}
