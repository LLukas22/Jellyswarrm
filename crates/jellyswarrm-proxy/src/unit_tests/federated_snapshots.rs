//! Federated catalog snapshot regressions through the owning application service.
use super::tests::{request, setup};
use crate::application::catalog::snapshots::{SnapshotRequest, SnapshotResponse};

#[tokio::test]
async fn a_mutation_fences_inflight_builds_without_invalidating_another_viewer() {
    let (state, _, sessions, _) = setup().await;
    let state = state.catalog_service();
    let prepare = |viewer: &str| {
        let mut request = request("/Items?Limit=1", &sessions);
        request.context = crate::application::context::RequestContext::new(
            &request.original_request,
            None,
            None,
            None,
            Some(crate::libraries::service::VirtualLibraryAccessScope::new(
                viewer,
                [],
            )),
        );
        request
    };
    let old = prepare("viewer");
    let old = SnapshotRequest::prepare(&state, &old, &[], "listing").await;
    let other = prepare("other");
    let other = SnapshotRequest::prepare(&state, &other, &[], "listing").await;
    let value = |name| SnapshotResponse {
        response: serde_json::json!({"Items": [{"Id": name}], "TotalRecordCount": 1, "StartIndex": 0}),
        report: crate::application::catalog::scan::CatalogReport::new(Vec::new(), 0),
    };
    let _ = other
        .serve(&state, async { Ok(value("other")) })
        .await
        .unwrap();
    let started = tokio::sync::Notify::new();
    let release = tokio::sync::Notify::new();
    let old_build = old.serve(&state, async {
        started.notify_one();
        release.notified().await;
        Ok(value("old"))
    });
    let mutation = async {
        started.notified().await;
        state.catalog_snapshots.invalidate_viewer("viewer");
        release.notify_one();
    };
    let (old, ()) = tokio::join!(old_build, mutation);
    assert_eq!(old.unwrap()["Items"][0]["Id"], "old");
    let fresh = prepare("viewer");
    let fresh = SnapshotRequest::prepare(&state, &fresh, &[], "listing").await;
    assert_eq!(
        fresh
            .serve(&state, async { Ok(value("new")) })
            .await
            .unwrap()["Items"][0]["Id"],
        "new"
    );
    let other = prepare("other");
    let other = SnapshotRequest::prepare(&state, &other, &[], "listing").await;
    assert_eq!(
        other
            .serve(&state, async {
                panic!("another viewer's snapshot must survive")
            })
            .await
            .unwrap()["Items"][0]["Id"],
        "other"
    );
}
