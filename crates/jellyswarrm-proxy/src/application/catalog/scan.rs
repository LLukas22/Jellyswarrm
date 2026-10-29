//! Completeness tracking for catalog scans.
use crate::application::error::ApplicationError;
use crate::models::ItemsResponseVariants;

pub(super) const MAX_SCAN_ITEMS: usize = 100_000;
pub(super) const MAX_SCAN_PAGES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartialReason {
    Changed,
    Stalled,
    Failed,
    Deadline,
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchOutcome {
    Complete,
    BoundedFeed,
    Partial(PartialReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceOutcome {
    pub(crate) server_id: crate::domain::server_id::ServerId,
    pub(crate) parent_id: Option<crate::application::identity::BackendItemId>,
    pub(crate) outcome: FetchOutcome,
}

/// One authoritative fetch report. Unavailable targets are retained even when
/// planning could not supply their source identity. Totals/completeness are
/// derived, never independently mutable flags alongside these outcomes.
#[derive(Debug, Clone)]
pub(crate) struct CatalogReport {
    outcomes: Vec<SourceOutcome>,
    unavailable_targets: usize,
}

impl CatalogReport {
    pub(crate) fn new(outcomes: Vec<SourceOutcome>, unavailable_targets: usize) -> Self {
        Self {
            outcomes,
            unavailable_targets,
        }
    }
    pub(crate) fn outcomes(&self) -> &[SourceOutcome] {
        &self.outcomes
    }
    pub(crate) fn failure_count(&self) -> usize {
        self.unavailable_targets
            + self
                .outcomes
                .iter()
                .filter(|source| matches!(source.outcome, FetchOutcome::Partial(_)))
                .count()
    }
    pub(crate) fn is_successful(&self) -> bool {
        self.failure_count() == 0
    }
    pub(crate) fn is_complete_inventory(&self) -> bool {
        self.is_successful()
            && self
                .outcomes
                .iter()
                .all(|source| source.outcome == FetchOutcome::Complete)
    }
}

/// Shared validation for catalog and library-inventory scans. An uncounted
/// page is not proof of completion unless it is empty.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CountShape {
    Counted,
    Uncounted,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanStep {
    Continue,
    Complete,
}

#[derive(Default)]
pub(super) struct ScanProgress {
    paging: jellyfin_api::library_pagination::LibraryPagination,
    pages: usize,
    count_shape: Option<CountShape>,
}

impl ScanProgress {
    pub(crate) fn offset(&self) -> usize {
        self.paging.fetched_count()
    }

    #[cfg(test)]
    pub(crate) fn accept(&mut self, page: &ItemsResponseVariants) -> Result<bool, PartialReason> {
        self.accept_typed(page)
            .map(|step| step == ScanStep::Complete)
            .map_err(|error| match error {
                ApplicationError::IncompleteCatalog { reason, .. } => reason,
                _ => unreachable!("scan validation only produces incomplete inventory errors"),
            })
    }

    pub(crate) fn accept_typed(
        &mut self,
        page: &ItemsResponseVariants,
    ) -> Result<ScanStep, ApplicationError> {
        let counted = matches!(page, ItemsResponseVariants::WithCount(_));
        let shape = if counted {
            CountShape::Counted
        } else {
            CountShape::Uncounted
        };
        if self.count_shape.is_some_and(|previous| previous != shape)
            || page.items().iter().any(|item| item.id.is_empty())
        {
            return Err(incomplete(PartialReason::Changed));
        }
        self.count_shape = Some(shape);
        self.pages += 1;
        // A stable counted response already bounds the scan. Only uncounted
        // sources need a fixed work budget to detect an endless inventory.
        if !counted
            && (self.pages > MAX_SCAN_PAGES
                || self.offset().saturating_add(page.len()) > MAX_SCAN_ITEMS)
        {
            return Err(incomplete(PartialReason::Budget));
        }
        let total = match page {
            ItemsResponseVariants::WithCount(page) => {
                if page.start_index < 0
                    || page.start_index as usize != self.offset()
                    || page.total_record_count < 0
                {
                    return Err(incomplete(PartialReason::Changed));
                }
                Some(page.total_record_count as usize)
            }
            ItemsResponseVariants::Bare(_) => None,
        };
        let old_offset = self.offset();
        let complete = self
            .paging
            .accept_page(total, page.items().iter().map(|item| item.id.as_str()))
            .map_err(|error| ApplicationError::IncompleteCatalog {
                reason: if page.len() == 0 {
                    PartialReason::Stalled
                } else {
                    PartialReason::Changed
                },
                cause: Some(Box::new(ApplicationError::Internal(error.into()))),
            })?;
        if total.is_some_and(|total| self.offset() > total) {
            return Err(incomplete(PartialReason::Changed));
        }
        if self.offset() == old_offset && complete != Some(true) {
            return Err(incomplete(PartialReason::Stalled));
        }
        Ok(if complete == Some(true) {
            ScanStep::Complete
        } else {
            ScanStep::Continue
        })
    }
}

fn incomplete(reason: PartialReason) -> ApplicationError {
    ApplicationError::IncompleteCatalog {
        reason,
        cause: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn report_derives_completeness_without_losing_unavailable_targets() {
        let source = |outcome| SourceOutcome {
            server_id: crate::domain::server_id::ServerId::new(1),
            parent_id: Some("opaque-library".into()),
            outcome,
        };
        let complete = CatalogReport::new(vec![source(FetchOutcome::Complete)], 0);
        assert!(complete.is_successful());
        assert!(complete.is_complete_inventory());
        let bounded = CatalogReport::new(vec![source(FetchOutcome::BoundedFeed)], 0);
        assert!(bounded.is_successful());
        assert!(!bounded.is_complete_inventory());
        let unavailable = CatalogReport::new(vec![source(FetchOutcome::Complete)], 2);
        assert_eq!(unavailable.failure_count(), 2);
        assert!(!unavailable.is_successful());
        assert!(!unavailable.is_complete_inventory());
        let empty = CatalogReport::new(Vec::new(), 1);
        assert!(!empty.is_complete_inventory());
        let partial = CatalogReport::new(
            vec![source(FetchOutcome::Partial(PartialReason::Deadline))],
            1,
        );
        assert_eq!(partial.failure_count(), 2);
        assert_eq!(
            partial.outcomes()[0].outcome,
            FetchOutcome::Partial(PartialReason::Deadline)
        );
    }

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
