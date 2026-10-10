use std::collections::HashSet;

use crate::error::Error;

/// Completeness checks shared by library discovery and proxy inventory fetching.
/// HTTP requests and item metadata stay with the caller.
#[derive(Default)]
pub struct LibraryPagination {
    seen: HashSet<String>,
    expected_total: Option<usize>,
}

impl LibraryPagination {
    pub fn fetched_count(&self) -> usize {
        self.seen.len()
    }

    /// Returns a known completion status, or None when the upstream supplies no
    /// total. Callers then use their endpoint's unpaginated/short-page behavior.
    pub fn accept_page<'a>(
        &mut self,
        total: Option<usize>,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<Option<bool>, Error> {
        if let (Some(previous), Some(current)) = (self.expected_total, total) {
            if previous != current {
                return Err(Error::InvalidResponse(
                    "Library inventory changed while fetching its pages".into(),
                ));
            }
        }
        self.expected_total = total.or(self.expected_total);
        let previous_count = self.fetched_count();
        for id in ids {
            if !self.seen.insert(id.to_owned()) {
                return Err(Error::InvalidResponse(
                    "Library pagination returned duplicate libraries".into(),
                ));
            }
        }
        let complete = self
            .expected_total
            .map(|total| self.fetched_count() >= total);
        if self.fetched_count() == previous_count {
            if complete == Some(false) {
                return Err(Error::InvalidResponse(
                    "Library pagination ended before all libraries were returned".into(),
                ));
            }
            return Ok(Some(true));
        }
        Ok(complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_by_received_items_and_keeps_the_advertised_total() {
        let mut paging = LibraryPagination::default();
        assert_eq!(
            paging.accept_page(Some(3), ["a", "b"]).unwrap(),
            Some(false)
        );
        assert_eq!(paging.fetched_count(), 2);
        assert_eq!(paging.accept_page(None, ["c"]).unwrap(), Some(true));
    }

    #[test]
    fn missing_total_leaves_completion_to_the_endpoint() {
        let mut paging = LibraryPagination::default();
        assert_eq!(paging.accept_page(None, ["a"]).unwrap(), None);
        assert_eq!(paging.accept_page(None, []).unwrap(), Some(true));
    }

    #[test]
    fn rejects_repeated_ids_empty_continuations_and_changed_totals() {
        for (total, ids) in [
            (Some(3), vec!["a"]),
            (Some(3), vec![]),
            (Some(4), vec!["c"]),
        ] {
            let mut paging = LibraryPagination::default();
            paging.accept_page(Some(3), ["a", "b"]).unwrap();
            assert!(paging.accept_page(total, ids).is_err());
        }
    }

    #[test]
    fn accepts_an_empty_inventory() {
        assert_eq!(
            LibraryPagination::default()
                .accept_page(Some(0), [])
                .unwrap(),
            Some(true)
        );
    }
}
