use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::get,
    Router,
};
use jellyswarrm_macros::lowercase_routes;
use tower::ServiceExt;

async fn handler() -> &'static str {
    "ok"
}

#[tokio::test]
async fn test_simple_route_duplication() {
    let app = lowercase_routes! {
        Router::new()
            .route("/TestPath", get(handler))
    };

    // Test original path
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/TestPath")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Test lowercase path
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/testpath")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_nested_route_duplication() {
    let app = lowercase_routes! {
        Router::new()
            .nest("/Parent", Router::new()
                .route("/Child", get(handler)))
    };

    // Test original nested path
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Parent/Child")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Test lowercase nested path (parent lowercase)
    // IMPORTANT: The macro as implemented duplicates the `nest` call.
    // So Router::new().nest("/Parent", ...).nest("/parent", ...)
    // But the INNER router is compiled ONCE in the macro logic I saw.
    // Let's re-read the macro logic to be sure about recursive behavior.

    // If the macro is:
    /*
        if method_name == "nest" {
            if let Some(arg) = method_call.args.get_mut(1) {
                process_routes(arg);
            }
        }
    */
    // It processes the inner router first.
    // So inner router becomes: Router::new().route("/Child", ...).route("/child", ...)
    // Outer router becomes: Router::new().nest("/Parent", inner).nest("/parent", inner)

    // So valid paths should be:
    // /Parent/Child
    // /Parent/child  (inner processed)
    // /parent/Child  (outer processed)
    // /parent/child  (both processed)

    // Test /parent/child
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/parent/child")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Test /Parent/child
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Parent/child")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Test /parent/Child
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/parent/Child")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_no_uppercase_ignored() {
    let app = lowercase_routes! {
        Router::new()
            .route("/lowercase", get(handler))
    };

    // Original works
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/lowercase")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Uppercase should NOT work (macro shouldn't create uppercase from lowercase, nor should it allow uppercase to hit lowercase unless using a different middleware)
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Lowercase")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_complex_chain() {
    let app = lowercase_routes! {
        Router::new()
            .route("/A", get(handler))
            .nest("/B", Router::new()
                .route("/C", get(handler))
            )
            .route("/D", get(handler))
    };

    // Check various combinations
    let paths = vec!["/A", "/a", "/B/C", "/b/c", "/B/c", "/b/C", "/D", "/d"];

    for path in paths {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "Failed on path: {}",
            path
        );
    }
}

#[tokio::test]
async fn test_wildcards_and_params() {
    let app = lowercase_routes! {
        Router::new()
            .route("/Item/{id}", get(handler))
            .route("/Files/{*path}", get(handler))
    };

    // Test parameter route
    let paths = vec!["/Item/123", "/item/123"];

    for path in paths {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "Failed on path: {}",
            path
        );
    }

    // Test wildcard route
    let paths = vec!["/Files/some/long/path.txt", "/files/some/long/path.txt"];

    for path in paths {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "Failed on path: {}",
            path
        );
    }
}
