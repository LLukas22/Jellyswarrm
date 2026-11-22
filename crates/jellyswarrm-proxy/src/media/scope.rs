//! Represent access-scoped catalog identity and normalize media scope keys.
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaScopeKind {
    Configured,
    Automatic,
    Aggregate,
    Latest,
    Search,
    Resume,
    NextUp,
    /// Home-screen rows without a library, such as favorites.
    Row,
}

impl MediaScopeKind {
    /// Additive, viewer-first scopes whose library is optional.
    fn is_viewer_feed(self) -> bool {
        matches!(
            self,
            Self::Latest | Self::Search | Self::Resume | Self::Row | Self::NextUp
        )
    }
}

/// Shared codec for persisted catalog keys and viewer-scoped reverse lookups.
/// Keep the existing key layouts so published aggregate IDs remain usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MediaCatalogScope<'a> {
    pub(crate) kind: MediaScopeKind,
    pub(crate) viewer: &'a str,
    pub(crate) resource_id: &'a str,
}

impl<'a> MediaCatalogScope<'a> {
    pub(crate) fn parse(key: &'a str) -> Option<Self> {
        let mut parts = key.split(':');
        let kind = match parts.next()? {
            "configured" => MediaScopeKind::Configured,
            "automatic" => MediaScopeKind::Automatic,
            "aggregate" => MediaScopeKind::Aggregate,
            "latest" => MediaScopeKind::Latest,
            "search" => MediaScopeKind::Search,
            "resume" => MediaScopeKind::Resume,
            "row" => MediaScopeKind::Row,
            "nextup" => MediaScopeKind::NextUp,
            _ => return None,
        };
        let middle = parts.next()?;
        let last = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let (viewer, resource_id) = if kind.is_viewer_feed() {
            (middle, last)
        } else {
            (last, middle)
        };
        if viewer.is_empty() || (resource_id.is_empty() && !kind.is_viewer_feed()) {
            return None;
        }
        Some(Self {
            kind,
            viewer,
            resource_id,
        })
    }

    pub(crate) fn belongs_to(key: &str, viewer: &str) -> bool {
        MediaCatalogScope::parse(key).is_some_and(|scope| scope.viewer == viewer)
    }
}

impl fmt::Display for MediaCatalogScope<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            MediaScopeKind::Configured => "configured",
            MediaScopeKind::Automatic => "automatic",
            MediaScopeKind::Aggregate => "aggregate",
            MediaScopeKind::Latest => "latest",
            MediaScopeKind::Search => "search",
            MediaScopeKind::Resume => "resume",
            MediaScopeKind::NextUp => "nextup",
            MediaScopeKind::Row => "row",
        };
        if self.kind.is_viewer_feed() {
            write!(f, "{kind}:{}:{}", self.viewer, self.resource_id)
        } else {
            write!(f, "{kind}:{}:{}", self.resource_id, self.viewer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_scopes_round_trip_and_match_only_their_viewer() {
        for key in [
            "configured:library:viewer",
            "automatic:library:viewer",
            "aggregate:show:viewer",
            "latest:viewer:",
            "latest:viewer:library",
            "search:viewer:",
            "search:viewer:library",
            "resume:viewer:",
            "resume:viewer:library",
            "row:viewer:",
            "nextup:viewer:",
            "nextup:viewer:library",
        ] {
            let scope = MediaCatalogScope::parse(key).unwrap();
            assert_eq!(scope.to_string(), key);
            assert!(MediaCatalogScope::belongs_to(key, "viewer"));
            assert!(!MediaCatalogScope::belongs_to(key, "other"));
        }
        for key in [
            "unknown:viewer:",
            "search:viewer",
            "search::",
            "search:viewer:library:extra",
            "configured::viewer",
            "automatic:library:",
        ] {
            assert!(MediaCatalogScope::parse(key).is_none(), "{key}");
        }
    }
}
