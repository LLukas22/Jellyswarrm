use url::Url;
use uuid::Uuid;

pub fn is_id_like(segment: &str) -> bool {
    Uuid::parse_str(segment).is_ok()
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
    let mut url = url.clone();
    let path = url.path();
    url.set_path(&path.replace(original, replacement));
    url
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
