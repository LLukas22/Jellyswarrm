use super::postprocessing::Pagination;

pub(super) const UPSTREAM_PAGE_SIZE: usize = 100;

/// Leave other parent references for the per-server request processor.
pub(super) fn replace_aggregate_parent_id(
    url: &url::Url,
    resolved_id: &str,
    new_id: &str,
) -> url::Url {
    let pairs = url
        .query_pairs()
        .map(|(key, value)| {
            let value = if value == resolved_id
                && ["ParentId", "SeriesId", "SeasonId"]
                    .iter()
                    .any(|expected| key.eq_ignore_ascii_case(expected))
            {
                new_id.to_string()
            } else {
                value.into_owned()
            };
            (key.into_owned(), value)
        })
        .collect::<Vec<_>>();

    let mut new_url = url.clone();
    new_url.query_pairs_mut().clear().extend_pairs(pairs);
    new_url
}

pub(super) fn has_query_key(url: &url::Url, keys: &[&str]) -> bool {
    url.query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| {
                keys.iter()
                    .any(|expected_key| key.eq_ignore_ascii_case(expected_key))
            })
        })
        .unwrap_or(false)
}

pub(super) fn is_upstream_limited_catalog_request(url: &url::Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.contains("/latest") || path.contains("/suggestions")
}

pub(super) fn is_authoritative_media_inventory_request(url: &url::Url) -> bool {
    if is_upstream_limited_catalog_request(url)
        || !url
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .is_some_and(|segment| segment.eq_ignore_ascii_case("Items"))
    {
        return false;
    }
    const SAFE_KEYS: &[&str] = &[
        "parentid",
        "userid",
        "startindex",
        "limit",
        "recursive",
        "fields",
        "sortby",
        "sortorder",
        "includeitemtypes",
        "imagetypeLimit",
        "enableimagetypes",
        "enabletotalrecordcount",
    ];
    let mut recursive = false;
    let mut includes_dedup_types = true;
    let safe = url.query_pairs().all(|(key, value)| {
        if key.eq_ignore_ascii_case("recursive") {
            recursive = value.eq_ignore_ascii_case("true");
        } else if key.eq_ignore_ascii_case("includeitemtypes") {
            // A source shares sightings across types, so replacing it requires
            // coverage of every type participating in reconciliation.
            includes_dedup_types &=
                ["Movie", "Series", "Season", "Episode"]
                    .iter()
                    .all(|required| {
                        value
                            .split(',')
                            .any(|item_type| item_type.trim().eq_ignore_ascii_case(required))
                    });
        }
        SAFE_KEYS.iter().any(|safe| key.eq_ignore_ascii_case(safe))
    });
    safe && recursive && includes_dedup_types
}

pub(super) fn merged_library_max_pages(pagination: Pagination) -> Option<usize> {
    pagination.limit.map(|client_limit| {
        let window_end = pagination.start_index.saturating_add(client_limit);
        window_end
            .saturating_mul(3)
            .div_ceil(2)
            .div_ceil(UPSTREAM_PAGE_SIZE)
            .max(1)
    })
}

pub(super) fn set_upstream_page(url: &mut url::Url, start_index: usize, limit: usize) {
    let pairs = url
        .query_pairs()
        .filter(|(key, _)| !is_pagination_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();

    let mut query = url.query_pairs_mut();
    query.clear().extend_pairs(pairs);
    query
        .append_pair("StartIndex", &start_index.to_string())
        .append_pair("Limit", &limit.to_string());
}

pub(super) fn ensure_duplicate_identity_field(url: &mut url::Url) {
    ensure_item_fields(url, &["ProviderIds"]);
}

pub(super) fn ensure_global_sort_fields(url: &mut url::Url) {
    let sorts_by_date_created = url.query_pairs().any(|(key, value)| {
        key.eq_ignore_ascii_case("SortBy")
            && value
                .split(',')
                .map(str::trim)
                .any(|field| field.eq_ignore_ascii_case("DateCreated"))
    });
    if url.path().to_ascii_lowercase().ends_with("/latest") || sorts_by_date_created {
        ensure_item_fields(url, &["DateCreated"]);
    }
}

fn ensure_item_fields(url: &mut url::Url, required_fields: &[&str]) {
    for required_field in required_fields {
        crate::url_helper::ensure_query_list_value(url, "Fields", required_field);
    }
}

pub(super) fn normalize_upstream_pagination(url: &mut url::Url, pagination: Pagination) {
    let pairs = url
        .query_pairs()
        .filter(|(key, _)| !is_pagination_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();

    let upstream_limit = pagination
        .limit
        .map(|limit| pagination.start_index.saturating_add(limit));

    let mut query = url.query_pairs_mut();
    query.clear().extend_pairs(pairs);
    if let Some(upstream_limit) = upstream_limit {
        query.append_pair("Limit", &upstream_limit.to_string());
    }
}

fn is_pagination_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("StartIndex") || key.eq_ignore_ascii_case("Limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_key_matching_uses_keys_not_values_and_decodes_keys() {
        let value = url::Url::parse("http://localhost/Items?foo=ParentId").unwrap();
        let key = url::Url::parse("http://localhost/Items?parentid=abc").unwrap();
        let encoded = url::Url::parse("http://localhost/Items?Parent%49d=abc").unwrap();

        assert!(!has_query_key(&value, &["ParentId"]));
        assert!(has_query_key(&key, &["ParentId"]));
        assert!(has_query_key(&encoded, &["ParentId"]));
    }

    #[test]
    fn limited_catalog_request_detects_latest_and_suggestions() {
        let latest =
            url::Url::parse("http://localhost/Users/u/Items/Latest?Limit=16&ParentId=abc").unwrap();
        let suggestions =
            url::Url::parse("http://localhost/Users/u/Items/Suggestions?Limit=12").unwrap();
        let browse =
            url::Url::parse("http://localhost/Users/u/Items?Limit=100&ParentId=abc").unwrap();

        assert!(is_upstream_limited_catalog_request(&latest));
        assert!(is_upstream_limited_catalog_request(&suggestions));
        assert!(!is_upstream_limited_catalog_request(&browse));
    }

    #[test]
    fn merged_library_page_limit_scales_with_client_window() {
        assert_eq!(
            merged_library_max_pages(Pagination {
                start_index: 0,
                limit: Some(100),
            }),
            Some(2)
        );
        assert_eq!(
            merged_library_max_pages(Pagination {
                start_index: 100,
                limit: Some(100),
            }),
            Some(3)
        );
        assert_eq!(merged_library_max_pages(Pagination::unbounded()), None);
        assert_eq!(
            merged_library_max_pages(Pagination {
                start_index: 1_200,
                limit: Some(100),
            }),
            Some(20)
        );
    }

    #[test]
    fn upstream_pagination_fetches_enough_for_global_page() {
        let mut url = url::Url::parse(
            "http://localhost/Items?StartIndex=20&Limit=10&Recursive=true&Fields=Genres",
        )
        .unwrap();
        normalize_upstream_pagination(
            &mut url,
            Pagination {
                start_index: 20,
                limit: Some(10),
            },
        );

        assert_eq!(
            query_pairs(&url),
            vec![
                ("Recursive".to_string(), "true".to_string()),
                ("Fields".to_string(), "Genres".to_string()),
                ("Limit".to_string(), "30".to_string()),
            ]
        );
    }

    #[test]
    fn unbounded_upstream_pagination_removes_start_index() {
        let mut url =
            url::Url::parse("http://localhost/Items?StartIndex=20&Recursive=true").unwrap();
        normalize_upstream_pagination(
            &mut url,
            Pagination {
                start_index: 20,
                limit: None,
            },
        );

        assert_eq!(
            query_pairs(&url),
            vec![("Recursive".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn upstream_pagination_uses_saturating_limit_math() {
        let mut url = url::Url::parse("http://localhost/Items?Limit=1").unwrap();
        normalize_upstream_pagination(
            &mut url,
            Pagination {
                start_index: usize::MAX,
                limit: Some(10),
            },
        );

        assert_eq!(
            query_pairs(&url),
            vec![("Limit".to_string(), usize::MAX.to_string())]
        );
    }

    #[test]
    fn upstream_page_preserves_non_pagination_parameters() {
        let mut url = url::Url::parse(
            "http://localhost/Items?StartIndex=20&Limit=10&Recursive=true&Fields=Genres",
        )
        .unwrap();
        set_upstream_page(&mut url, 300, 100);

        assert_eq!(
            query_pairs(&url),
            vec![
                ("Recursive".to_string(), "true".to_string()),
                ("Fields".to_string(), "Genres".to_string()),
                ("StartIndex".to_string(), "300".to_string()),
                ("Limit".to_string(), "100".to_string()),
            ]
        );
    }

    #[test]
    fn duplicate_identity_field_only_adds_provider_ids() {
        let mut url = url::Url::parse("http://localhost/Items?Fields=Genres").unwrap();
        ensure_duplicate_identity_field(&mut url);

        assert_eq!(
            query_pairs(&url),
            vec![("Fields".to_string(), "Genres,ProviderIds".to_string())]
        );
    }

    #[test]
    fn only_recursive_media_catalogs_are_authoritative_inventories() {
        for query in [
            "ParentId=library&Recursive=true&Limit=100",
            "ParentId=library&Recursive=true&IncludeItemTypes=Movie,Series,Season,Episode",
        ] {
            let url = url::Url::parse(&format!("http://localhost/Items?{query}")).unwrap();
            assert!(is_authoritative_media_inventory_request(&url), "{query}");
        }

        for query in [
            "ParentId=library&Recursive=true&IncludeItemTypes=Movie&Limit=100",
            "ParentId=library&Recursive=true&IncludeItemTypes=Series",
            "ParentId=library&Recursive=true&IncludeItemTypes=Season",
            "ParentId=library&Recursive=true&IncludeItemTypes=Episode",
            "ParentId=library&Recursive=true&IncludeItemTypes=Movie,Series,Episode",
            "ParentId=library&Recursive=false&IncludeItemTypes=Movie",
            "ParentId=library&Recursive=true&IncludeItemTypes=Audio",
            "ParentId=library&Recursive=true&IncludeItemTypes=Movie&SearchTerm=Alien",
        ] {
            let url = url::Url::parse(&format!("http://localhost/Items?{query}")).unwrap();
            assert!(!is_authoritative_media_inventory_request(&url), "{query}");
        }

        let resume = url::Url::parse(
            "http://localhost/Users/user/Items/Resume?Recursive=true&IncludeItemTypes=Movie",
        )
        .unwrap();
        assert!(!is_authoritative_media_inventory_request(&resume));
    }

    #[test]
    fn show_children_are_additive_even_when_unfiltered() {
        for endpoint in ["Seasons", "Episodes"] {
            for query in [
                "",
                "?SeasonId=season",
                "?IsMissing=false",
                "?IsPlayed=true",
                "?SearchTerm=pilot",
            ] {
                let url =
                    url::Url::parse(&format!("http://localhost/Shows/show/{endpoint}{query}"))
                        .unwrap();
                assert!(!is_authoritative_media_inventory_request(&url));
            }
        }
    }

    #[test]
    fn aggregate_parent_replacement_only_changes_exact_resolved_values() {
        let url = url::Url::parse(
            "http://localhost/Items?parentid=show&SeriesId=show&SeasonId=season&Other=show",
        )
        .unwrap();
        let replaced = replace_aggregate_parent_id(&url, "show", "backend-show");
        assert_eq!(
            query_pairs(&replaced),
            vec![
                ("parentid".into(), "backend-show".into()),
                ("SeriesId".into(), "backend-show".into()),
                ("SeasonId".into(), "season".into()),
                ("Other".into(), "show".into()),
            ]
        );
        let replaced = replace_aggregate_parent_id(&url, "season", "backend-season");
        assert_eq!(
            replaced
                .query_pairs()
                .find(|(key, _)| key == "SeriesId")
                .unwrap()
                .1,
            "show"
        );
        assert_eq!(
            replaced
                .query_pairs()
                .find(|(key, _)| key == "SeasonId")
                .unwrap()
                .1,
            "backend-season"
        );
    }

    #[test]
    fn latest_requests_include_date_created_for_global_sorting() {
        let mut url = url::Url::parse(
            "http://localhost/Users/u/Items/Latest?Limit=8&Fields=PrimaryImageAspectRatio,Path",
        )
        .unwrap();
        ensure_global_sort_fields(&mut url);

        assert_eq!(
            query_pairs(&url),
            vec![
                ("Limit".to_string(), "8".to_string()),
                (
                    "Fields".to_string(),
                    "PrimaryImageAspectRatio,Path,DateCreated".to_string(),
                ),
            ]
        );
    }

    fn query_pairs(url: &url::Url) -> Vec<(String, String)> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }
}
