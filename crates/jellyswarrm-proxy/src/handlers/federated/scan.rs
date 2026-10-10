use crate::models::ItemsResponseVariants;

pub(super) const MAX_SCAN_ITEMS: usize = 100_000;
pub(super) const MAX_SCAN_PAGES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PartialReason {
    Changed,
    Stalled,
    Failed,
    Deadline,
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FetchOutcome {
    Complete,
    BoundedFeed,
    Partial(PartialReason),
}

/// Shared validation for catalog and library-inventory scans. An uncounted
/// page is not proof of completion unless it is empty.
#[derive(Default)]
pub(super) struct ScanProgress {
    paging: jellyfin_api::library_pagination::LibraryPagination,
    pages: usize,
    counted: Option<bool>,
}

impl ScanProgress {
    pub fn offset(&self) -> usize {
        self.paging.fetched_count()
    }

    pub fn accept(&mut self, page: &ItemsResponseVariants) -> Result<bool, PartialReason> {
        let counted = matches!(page, ItemsResponseVariants::WithCount(_));
        if self.counted.is_some_and(|previous| previous != counted)
            || page.items().iter().any(|item| item.id.is_empty())
        {
            return Err(PartialReason::Changed);
        }
        self.counted = Some(counted);
        self.pages += 1;
        // A stable counted response already bounds the scan. Only uncounted
        // sources need a fixed work budget to detect an endless inventory.
        if !counted
            && (self.pages > MAX_SCAN_PAGES
                || self.offset().saturating_add(page.len()) > MAX_SCAN_ITEMS)
        {
            return Err(PartialReason::Budget);
        }
        let total = match page {
            ItemsResponseVariants::WithCount(page) => {
                if page.start_index < 0
                    || page.start_index as usize != self.offset()
                    || page.total_record_count < 0
                {
                    return Err(PartialReason::Changed);
                }
                Some(page.total_record_count as usize)
            }
            ItemsResponseVariants::Bare(_) => None,
        };
        let old_offset = self.offset();
        let complete = self
            .paging
            .accept_page(total, page.items().iter().map(|item| item.id.as_str()))
            .map_err(|_| {
                if page.len() == 0 {
                    PartialReason::Stalled
                } else {
                    PartialReason::Changed
                }
            })?;
        if total.is_some_and(|total| self.offset() > total) {
            return Err(PartialReason::Changed);
        }
        if self.offset() == old_offset && complete != Some(true) {
            return Err(PartialReason::Stalled);
        }
        Ok(complete == Some(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(ids: &[&str], total: i32, offset: i32) -> ItemsResponseVariants {
        serde_json::from_value(json!({"Items": ids.iter().map(|id| json!({"Id": id, "Type": "Movie"})).collect::<Vec<_>>(), "TotalRecordCount": total, "StartIndex": offset})).unwrap()
    }

    #[test]
    fn rejects_changed_inventories_and_false_completion() {
        for next in [
            page(&["c"], 4, 2),
            page(&["c"], 3, 0),
            page(&["b"], 3, 2),
            page(&[], 3, 2),
            page(&["c", "d"], 3, 2),
            page(&[""], 3, 2),
            page(&["c"], -1, 2),
            ItemsResponseVariants::Bare(Vec::new()),
        ] {
            let mut progress = ScanProgress::default();
            assert!(!progress.accept(&page(&["a", "b"], 3, 0)).unwrap());
            assert!(progress.accept(&next).is_err(), "{next:?}");
        }
    }

    #[test]
    fn caps_work_and_accepts_short_pages_only_with_completion_evidence() {
        let mut progress = ScanProgress::default();
        assert!(!progress.accept(&page(&["a"], 2, 0)).unwrap());
        assert!(progress.accept(&page(&["b"], 2, 1)).unwrap());
        let mut progress = ScanProgress::default();
        assert!(!progress
            .accept(&ItemsResponseVariants::Bare(
                page(&["a"], 1, 0).into_items()
            ))
            .unwrap());
        assert!(progress
            .accept(&ItemsResponseVariants::Bare(Vec::new()))
            .unwrap());
        progress.pages = MAX_SCAN_PAGES;
        assert_eq!(
            progress.accept(&ItemsResponseVariants::Bare(Vec::new())),
            Err(PartialReason::Budget)
        );
        let mut progress = ScanProgress::default();
        let oversized = ItemsResponseVariants::Bare(
            (0..=MAX_SCAN_ITEMS)
                .map(|index| {
                    serde_json::from_value(json!({"Id": index.to_string(), "Type": "Movie"}))
                        .unwrap()
                })
                .collect(),
        );
        assert_eq!(progress.accept(&oversized), Err(PartialReason::Budget));
        // Large, counted libraries must remain browsable past the defensive
        // limits intended for sources that provide no completion evidence.
        let counted = ItemsResponseVariants::WithCount(crate::models::ItemsResponseWithCount {
            total_record_count: oversized.len() as i32,
            start_index: 0,
            items: oversized.into_items(),
        });
        let mut progress = ScanProgress {
            pages: MAX_SCAN_PAGES,
            ..Default::default()
        };
        assert!(progress.accept(&counted).unwrap());
    }
}
