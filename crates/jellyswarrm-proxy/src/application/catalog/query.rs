//! Canonical catalog intent. The wire request remains intact for translation;
//! client pagination is not part of materialized query identity.
use super::{postprocessing::Pagination, request_policy::CatalogRequestPolicy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogQueryKind {
    Roots,
    Items,
    ShowChildren,
    Batch,
    Guide,
    Feed,
}

pub(crate) struct CatalogQuery {
    pub(crate) kind: CatalogQueryKind,
    pub(super) pagination: Pagination,
    // Retain every extension and duplicate value, rather than reconstructing a
    // query from the fields understood by this proxy.
    path: String,
    parameters: Vec<(String, String)>,
}

impl CatalogQuery {
    pub(crate) fn from_url(url: &url::Url) -> Self {
        let path = url.path().trim_end_matches('/').to_ascii_lowercase();
        let kind = if path.ends_with("/views")
            || path.ends_with("/userviews")
            || path.ends_with("/mediafolders")
        {
            CatalogQueryKind::Roots
        } else if path.contains("/shows/")
            && (path.ends_with("/seasons") || path.ends_with("/episodes"))
        {
            CatalogQueryKind::ShowChildren
        } else if url
            .query_pairs()
            .any(|(key, _)| key.eq_ignore_ascii_case("Ids"))
        {
            CatalogQueryKind::Batch
        } else if path.contains("/livetv/") {
            CatalogQueryKind::Guide
        } else if super::request_policy::is_upstream_limited_catalog_request(url)
            || path.ends_with("/resume")
            || path.ends_with("/nextup")
        {
            CatalogQueryKind::Feed
        } else {
            CatalogQueryKind::Items
        };
        let mut pairs = url
            .query_pairs()
            .filter(|(key, _)| {
                !key.eq_ignore_ascii_case("StartIndex") && !key.eq_ignore_ascii_case("Limit")
            })
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        // Jellyfin keys are case-insensitive. Stable grouping must preserve the
        // relative order of mixed-case duplicates (notably SortBy/SortOrder),
        // because their value order determines primary and secondary criteria.
        pairs.sort_by_key(|(key, _)| key.to_ascii_lowercase());
        Self {
            kind,
            pagination: Pagination::from_url(url),
            path: url.path().to_string(),
            parameters: pairs,
        }
    }

    /// Request-independent, exhaustive inventory intent. This is not a client
    /// request template: credentials, origin, headers and transport are absent.
    pub(super) fn inventory(parent_id: &str) -> Self {
        Self {
            kind: CatalogQueryKind::Items,
            pagination: Pagination::unbounded(),
            path: "/Items".into(),
            parameters: vec![
                (
                    "IncludeItemTypes".into(),
                    "Movie,Series,Season,Episode".into(),
                ),
                ("ParentId".into(), parent_id.into()),
                ("Recursive".into(), "true".into()),
            ],
        }
    }

    pub(crate) fn identity(&self) -> String {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&self.parameters)
            .finish();
        format!("{}?{query}", self.path)
    }

    /// Materialize only at a wire/policy boundary. The query itself does not
    /// require an HTTP request or a placeholder incoming origin.
    pub(super) fn to_url(&self, mut base: url::Url) -> url::Url {
        base.set_path(&self.path);
        base.set_query(None);
        base.set_fragment(None);
        base.query_pairs_mut().extend_pairs(&self.parameters);
        base
    }

    pub(crate) fn policy(&self, materialize: bool) -> CatalogRequestPolicy {
        let url =
            self.to_url(url::Url::parse("http://catalog.invalid").expect("static policy origin"));
        let mut policy = CatalogRequestPolicy::from_url(&url);
        policy.pagination = if materialize {
            Pagination::unbounded()
        } else {
            self.pagination
        };
        policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_share_identity_without_losing_extensions_or_mutating_input() {
        let first =
            url::Url::parse("http://proxy/Items?X-Future=a&Limit=2&ParentId=library&X-Future=b")
                .unwrap();
        let second = url::Url::parse(
            "http://proxy/Items?ParentId=library&StartIndex=2&X-Future=a&X-Future=b&Limit=7",
        )
        .unwrap();
        let query = CatalogQuery::from_url(&first);
        assert_eq!(query.identity(), CatalogQuery::from_url(&second).identity());
        assert_eq!(
            query
                .parameters
                .iter()
                .filter(|(key, _)| key == "X-Future")
                .count(),
            2
        );
        assert_eq!(query.pagination.limit, Some(2));
        assert!(query.policy(true).pagination.limit.is_none());
        assert!(first.query().unwrap().contains("Limit=2"));
    }

    #[test]
    fn canonical_policy_preserves_mixed_case_duplicate_sort_precedence() {
        use super::super::postprocessing::SortPolicy;
        let original = url::Url::parse("http://proxy/Items?sortBy=AlbumArtist&SortBy=DateCreated,SortName&sortOrder=Ascending&SortOrder=Descending,Ascending").unwrap();
        let query = CatalogQuery::from_url(&original);
        let SortPolicy::Fields(expected) = SortPolicy::from_url(&original) else {
            panic!("expected field sorting")
        };
        for materialize in [false, true] {
            let SortPolicy::Fields(actual) = query.policy(materialize).sort else {
                panic!("expected field sorting")
            };
            assert_eq!(actual, expected);
        }
        let reversed = url::Url::parse("http://proxy/Items?SortBy=DateCreated,SortName&sortBy=AlbumArtist&SortOrder=Descending,Ascending&sortOrder=Ascending").unwrap();
        assert_ne!(
            query.identity(),
            CatalogQuery::from_url(&reversed).identity()
        );
    }

    #[test]
    fn operation_kinds_are_explicit() {
        for (path, kind) in [
            ("/Users/u/Views", CatalogQueryKind::Roots),
            ("/Items", CatalogQueryKind::Items),
            ("/Items?Ids=a,b", CatalogQueryKind::Batch),
            ("/Shows/s/Episodes", CatalogQueryKind::ShowChildren),
            ("/LiveTv/Programs", CatalogQueryKind::Guide),
            ("/Items/Latest", CatalogQueryKind::Feed),
        ] {
            assert_eq!(
                CatalogQuery::from_url(&url::Url::parse(&format!("http://proxy{path}")).unwrap())
                    .kind,
                kind
            );
        }
    }
}
