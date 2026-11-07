//! Canonical catalog intent and the single owner of catalog route interpretation.
//! To support another endpoint or alias, extend [`CatalogEndpoint::from_path`];
//! policy, planning, fetching and snapshots all read that classification.
//! The wire request remains intact for translation; client pagination is not
//! part of materialized query identity.
use super::{postprocessing::Pagination, request_policy::CatalogRequestPolicy};
use crate::application::{
    error::ApplicationError, media_services::MediaServices, request::input::OperationRequest,
};

/// What a catalog path asks for. Matching only the trailing segments keeps this
/// independent of the configured URL prefix, and matching whole segments keeps
/// a prefix or item ID such as `latest-items` from selecting an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogEndpoint {
    /// `/Users/{id}/Views`, `/UserViews`
    UserViews,
    /// `/Library/MediaFolders`
    MediaFolders,
    /// `/Items`, `/Users/{id}/Items`
    Items,
    /// `/Items/Latest`, `/Users/{id}/Items/Latest`
    Latest,
    /// `/Items/Suggestions`, `/Users/{id}/Items/Suggestions`
    Suggestions,
    /// `/UserItems/Resume`, `/Users/{id}/Items/Resume`
    Resume,
    /// `/Shows/NextUp`
    NextUp,
    /// `/Shows/{id}/Seasons`
    ShowSeasons,
    /// `/Shows/{id}/Episodes`
    ShowEpisodes,
    /// `/LiveTv/Programs`
    LiveTvPrograms,
    /// Listings without endpoint-specific policy, such as `/Genres`.
    Other,
}

impl CatalogEndpoint {
    pub(super) fn from_url(url: &url::Url) -> Self {
        Self::from_path(url.path())
    }

    fn from_path(path: &str) -> Self {
        let path = path.to_ascii_lowercase();
        let segments = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        match segments.as_slice() {
            [.., "users", _, "views"] | [.., "userviews"] => Self::UserViews,
            [.., "library", "mediafolders"] => Self::MediaFolders,
            [.., "items", "latest"] => Self::Latest,
            [.., "items", "suggestions"] => Self::Suggestions,
            [.., "items" | "useritems", "resume"] => Self::Resume,
            [.., "shows", "nextup"] => Self::NextUp,
            [.., "shows", _, "seasons"] => Self::ShowSeasons,
            [.., "shows", _, "episodes"] => Self::ShowEpisodes,
            [.., "livetv", "programs"] => Self::LiveTvPrograms,
            [.., "items"] => Self::Items,
            _ => Self::Other,
        }
    }

    pub(super) fn is_library_root(self) -> bool {
        matches!(self, Self::UserViews | Self::MediaFolders)
    }

    /// The upstream caps these itself, so a fetch returns a bounded window
    /// rather than an inventory that can be scanned page by page.
    pub(super) fn is_upstream_limited(self) -> bool {
        matches!(self, Self::Latest | Self::Suggestions)
    }

    /// Windowed feeds rather than listings of a stable collection.
    pub(super) fn is_feed(self) -> bool {
        self.is_upstream_limited() || matches!(self, Self::Resume | Self::NextUp)
    }

    /// The listing `Resume` filters, in the spelling and under the prefix of
    /// `path`. `None` once `path` no longer addresses `Resume`, so preparing an
    /// already rewritten URL again changes nothing.
    pub(super) fn resume_listing_path(self, path: &str) -> Option<String> {
        (self == Self::Resume && Self::from_path(path) == Self::Resume).then(|| {
            let base = parent_path(path);
            if last_segment(base).eq_ignore_ascii_case("UserItems") {
                format!("{}/Items", parent_path(base))
            } else {
                base.to_string()
            }
        })
    }

    /// `/Library/MediaFolders` as the same viewer's `/Users/{id}/Views`.
    pub(super) fn media_folders_views_path(path: &str, user_id: &str) -> String {
        format!("{}/Users/{user_id}/Views", parent_path(parent_path(path)))
    }
}

fn parent_path(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent)
}

fn last_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(crate) struct CatalogQuery {
    endpoint: CatalogEndpoint,
    pub(super) pagination: Pagination,
    // Retain every extension and duplicate value, rather than reconstructing a
    // query from the fields understood by this proxy.
    path: String,
    parameters: Vec<(String, String)>,
}

impl CatalogQuery {
    pub(crate) fn from_url(url: &url::Url) -> Self {
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
            endpoint: CatalogEndpoint::from_url(url),
            pagination: Pagination::from_url(url),
            path: url.path().to_string(),
            parameters: pairs,
        }
    }

    /// Request-independent, exhaustive inventory intent. This is not a client
    /// request template: credentials, origin, headers and transport are absent.
    pub(super) fn inventory(parent_id: &str) -> Self {
        Self {
            endpoint: CatalogEndpoint::Items,
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

    /// Materialize only at the wire boundary. The query itself does not require
    /// an HTTP request or a placeholder incoming origin.
    pub(super) fn to_url(&self, mut base: url::Url) -> url::Url {
        base.set_path(&self.path);
        base.set_query(None);
        base.set_fragment(None);
        base.query_pairs_mut().extend_pairs(&self.parameters);
        base
    }

    pub(crate) fn endpoint(&self) -> CatalogEndpoint {
        self.endpoint
    }

    /// Client parameters in canonical order, excluding pagination. Duplicate
    /// keys keep their relative order.
    pub(super) fn parameters(&self) -> &[(String, String)] {
        &self.parameters
    }

    /// `Ids` selects individual items only on the item listing. Elsewhere it is
    /// an ordinary filter forwarded with the endpoint's own query.
    pub(super) fn is_batch_lookup(&self) -> bool {
        self.endpoint == CatalogEndpoint::Items
            && self
                .parameters
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("Ids"))
    }

    pub(crate) fn policy(&self, materialize: bool) -> CatalogRequestPolicy {
        CatalogRequestPolicy::from_query(
            self,
            if materialize {
                Pagination::unbounded()
            } else {
                self.pagination
            },
        )
    }
}

/// Catalog view of an unselected operation. Authentication and authorization do not pick a backend.
impl OperationRequest {
    /// The preserved wire request is authoritative. Do not retain a second,
    /// independently mutable interpretation that can diverge after adaptation.
    pub(crate) fn catalog_query(&self) -> CatalogQuery {
        CatalogQuery::from_url(self.original_request.url())
    }

    pub(crate) async fn validate_catalog(
        &self,
        state: &MediaServices,
    ) -> Result<(), ApplicationError> {
        let mut access_url = self.original_request.url().clone();
        if CatalogEndpoint::from_url(&access_url) == CatalogEndpoint::Items {
            // Batch lookup authorizes/partitions each Id in RequestedItems.
            // Its established contract returns authorized results and omits
            // forbidden entries, rather than rejecting the entire batch.
            let pairs = access_url
                .query_pairs()
                .filter(|(key, _)| !key.eq_ignore_ascii_case("Ids"))
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            access_url.query_pairs_mut().clear().extend_pairs(pairs);
        }
        self.validate_access(&access_url, &state.url_selection)
            .await
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
        let single_keys = url::Url::parse("http://proxy/Items?SortBy=AlbumArtist,DateCreated,SortName&SortOrder=Ascending,Descending,Ascending").unwrap();
        let SortPolicy::Fields(expected) = CatalogQuery::from_url(&single_keys).policy(false).sort
        else {
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

    fn endpoint(path: &str) -> CatalogEndpoint {
        CatalogEndpoint::from_path(path)
    }

    #[test]
    fn every_catalog_route_has_one_classification() {
        use CatalogEndpoint::*;
        for (path, expected) in [
            ("/Users/u/Views", UserViews),
            ("/UserViews", UserViews),
            ("/Library/MediaFolders", MediaFolders),
            ("/Items", Items),
            ("/Users/u/Items", Items),
            ("/Items/Latest", Latest),
            ("/Users/u/Items/Latest", Latest),
            ("/Items/Suggestions", Suggestions),
            ("/Users/u/Items/Suggestions", Suggestions),
            ("/UserItems/Resume", Resume),
            ("/Users/u/Items/Resume", Resume),
            ("/Shows/NextUp", NextUp),
            ("/Shows/s/Seasons", ShowSeasons),
            ("/Shows/s/Episodes", ShowEpisodes),
            ("/LiveTv/Programs", LiveTvPrograms),
            ("/LiveTv/Programs/Recommended", Other),
            ("/LiveTv/Channels", Other),
            ("/Items/Filters", Other),
            ("/Genres", Other),
        ] {
            assert_eq!(endpoint(path), expected, "{path}");
            for spelling in [
                format!("/jellyfin{path}"),
                format!("/a/b{path}/"),
                path.to_ascii_lowercase(),
            ] {
                assert_eq!(endpoint(&spelling), expected, "{spelling}");
            }
        }
    }

    #[test]
    fn prefixes_and_item_ids_cannot_select_an_endpoint() {
        use CatalogEndpoint::*;
        for (path, expected) in [
            ("/latest-proxy/Items", Items),
            ("/suggestions/Items", Items),
            ("/livetv/Items", Items),
            ("/shows/Items", Items),
            ("/resume/Users/u/Views", UserViews),
            ("/Shows/latest/Episodes", ShowEpisodes),
            ("/Shows/nextup/Seasons", ShowSeasons),
            ("/Shows/items/Episodes", ShowEpisodes),
        ] {
            assert_eq!(endpoint(path), expected, "{path}");
        }
    }

    #[test]
    fn routes_declare_their_inventory_and_window_behavior() {
        for (path, library_root, upstream_limited, feed) in [
            ("/Users/u/Views", true, false, false),
            ("/UserViews", true, false, false),
            ("/Library/MediaFolders", true, false, false),
            ("/Items/Latest", false, true, true),
            ("/Users/u/Items/Suggestions", false, true, true),
            ("/UserItems/Resume", false, false, true),
            ("/Shows/NextUp", false, false, true),
            ("/Users/u/Items", false, false, false),
            ("/Shows/s/Episodes", false, false, false),
            ("/LiveTv/Programs", false, false, false),
            ("/Genres", false, false, false),
        ] {
            let route = endpoint(path);
            assert_eq!(route.is_library_root(), library_root, "{path}");
            assert_eq!(route.is_upstream_limited(), upstream_limited, "{path}");
            assert_eq!(route.is_feed(), feed, "{path}");
        }
    }

    #[test]
    fn ids_select_a_batch_lookup_only_on_the_item_listing() {
        for (path, expected) in [
            ("/Items?ids=a,b", true),
            ("/Users/u/Items?Ids=a", true),
            ("/Items", false),
            ("/Items?Filter=Ids", false),
            ("/Items/Latest?Ids=a", false),
            ("/Users/u/Views?Ids=a", false),
            ("/Shows/s/Episodes?Ids=a", false),
            ("/LiveTv/Programs?Ids=a", false),
        ] {
            let url = url::Url::parse(&format!("http://proxy{path}")).unwrap();
            assert_eq!(
                CatalogQuery::from_url(&url).is_batch_lookup(),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn materialized_query_forwards_unknown_parameters_and_their_duplicates() {
        let url = url::Url::parse(
            "http://proxy/jellyfin/Items?X-Future=a&Limit=2&x-future=b&Other=%20c&StartIndex=1",
        )
        .unwrap();
        let wire = CatalogQuery::from_url(&url).to_url(url::Url::parse("http://server").unwrap());
        assert_eq!(wire.path(), "/jellyfin/Items");
        assert_eq!(
            wire.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>(),
            [("Other", " c"), ("X-Future", "a"), ("x-future", "b")]
                .map(|(key, value)| (key.to_string(), value.to_string()))
        );
    }

    #[test]
    fn resume_listing_keeps_the_client_prefix_and_spelling() {
        use CatalogEndpoint::Resume;
        for (path, expected) in [
            ("/Users/u/Items/Resume", "/Users/u/Items"),
            ("/jellyfin/users/u/items/resume/", "/jellyfin/users/u/items"),
            ("/UserItems/Resume", "/Items"),
            ("/jellyfin/useritems/resume", "/jellyfin/Items"),
        ] {
            assert_eq!(
                Resume.resume_listing_path(path).as_deref(),
                Some(expected),
                "{path}"
            );
            assert_eq!(Resume.resume_listing_path(expected), None, "{expected}");
        }
        assert_eq!(
            CatalogEndpoint::Latest.resume_listing_path("/UserItems/Resume"),
            None
        );
    }

    #[test]
    fn media_folders_become_the_same_viewers_views_under_the_prefix() {
        for (path, expected) in [
            ("/Library/MediaFolders", "/Users/u/Views"),
            ("/jellyfin/Library/MediaFolders", "/jellyfin/Users/u/Views"),
            ("/library/mediafolders/", "/Users/u/Views"),
        ] {
            let rewritten = CatalogEndpoint::media_folders_views_path(path, "u");
            assert_eq!(rewritten, expected, "{path}");
            assert_eq!(endpoint(&rewritten), CatalogEndpoint::UserViews);
        }
    }
}
