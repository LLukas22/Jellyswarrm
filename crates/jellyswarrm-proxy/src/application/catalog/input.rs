//! Catalog view of an unselected operation. Authentication and authorization do not pick a backend.
use super::{query::CatalogQuery, services::CatalogService};
use crate::application::{error::ApplicationError, input::OperationRequest};

impl OperationRequest {
    /// The preserved wire request is authoritative. Do not retain a second,
    /// independently mutable interpretation that can diverge after adaptation.
    pub(crate) fn catalog_query(&self) -> CatalogQuery {
        CatalogQuery::from_url(self.original_request.url())
    }

    pub(crate) async fn validate_catalog(
        &self,
        state: &CatalogService,
    ) -> Result<(), ApplicationError> {
        let mut access_url = self.original_request.url().clone();
        if access_url
            .path()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .ends_with("/items")
        {
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
        self.validate_access(&access_url, &state.selection).await
    }
}
