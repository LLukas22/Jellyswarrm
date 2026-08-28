use url::Url;
use uuid::Uuid;

pub fn is_id_like(segment: &str) -> bool {
    Uuid::parse_str(segment).is_ok()
}

/// Joins a server URL with a request path, preserving any subdirectories in the server URL
///
/// # Examples
///
/// ```
/// use url::Url;
/// let server_url = Url::parse("http://server.com/jellyfin").unwrap();
/// let request_path = "/Users/123";
/// let result = join_server_url(&server_url, request_path);
/// assert_eq!(result.as_str(), "http://server.com/jellyfin/Users/123");
/// ```
pub fn join_server_url(server_url: &Url, request_path: &str) -> Url {
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

pub fn contains_id(url: &Url, name: &str) -> Option<String> {
    let segments: Vec<&str> = match url.path_segments() {
        Some(segments) => segments.collect(),
        None => Vec::new(),
    };

    let mut i = 0;

    while i < segments.len() {
        if i + 1 < segments.len() {
            let current = segments[i];
            let next = segments[i + 1];

            if current.eq_ignore_ascii_case(name) && is_id_like(next) {
                return Some(next.to_string());
            }
        }
        i += 1;
    }
    None
}

pub fn replace_id(url: Url, original: &str, replacement: &str) -> Url {
    let mut url = url;
    let Some(segments) = url.path_segments() else {
        return url;
    };

    let replaced_segments = segments
        .map(|segment| {
            if segment == original {
                replacement
            } else {
                segment
            }
        })
        .collect::<Vec<_>>();

    url.set_path(&replaced_segments.join("/"));
    url
}

/// Replaces the media ID immediately following `path_tag` without requiring
/// callers to know the request's full route shape.
pub fn replace_path_id(url: &Url, path_tag: &str, replacement: &str) -> Option<Url> {
    let original = contains_id(url, path_tag)?;
    Some(replace_id(url.clone(), &original, replacement))
}

/// Ensures a case-insensitive value is present in a comma-separated query
/// parameter while preserving all unrelated parameters.
pub fn ensure_query_list_value(url: &mut Url, expected_key: &str, value: &str) {
    let mut pairs = url
        .query_pairs()
        .map(|(key, entry)| (key.into_owned(), entry.into_owned()))
        .collect::<Vec<_>>();

    if let Some((_key, entries)) = pairs
        .iter_mut()
        .find(|(key, _entries)| key.eq_ignore_ascii_case(expected_key))
    {
        if entries
            .split(',')
            .any(|entry| entry.trim().eq_ignore_ascii_case(value))
        {
            return;
        }
        entries.push(',');
        entries.push_str(value);
    } else {
        pairs.push((expected_key.to_string(), value.to_string()));
    }

    url.query_pairs_mut().clear().extend_pairs(pairs);
}

#[cfg(test)]
mod tests {
    use super::*;

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
