//! Root-level monitoring routes, independent of authentication and upstream servers.
use axum::{extract::State, http::StatusCode, routing::get, Router};
use sqlx::SqlitePool;
use std::time::Duration;

pub fn router(pool: SqlitePool) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .with_state(pool)
}

async fn health() -> &'static str {
    "healthy"
}

async fn ready(State(pool): State<SqlitePool>) -> (StatusCode, &'static str) {
    // Serving starts only after initialization and migrations complete. Check that
    // the shared database remains usable, bounding both pool acquisition and SQL.
    match tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query("SELECT 1").execute(&pool),
    )
    .await
    {
        Ok(Ok(_)) => (StatusCode::OK, "ready"),
        Ok(Err(error)) => {
            tracing::warn!(%error, "Readiness database check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready")
        }
        Err(_) => {
            tracing::warn!("Readiness database check timed out");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    async fn check(app: Router, path: &str, status: StatusCode, body: &str) {
        let response = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), body.as_bytes());
    }

    #[tokio::test]
    async fn probes_succeed_without_authentication_or_configured_servers() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let app = router(pool);
        check(app.clone(), "/health", StatusCode::OK, "healthy").await;
        check(app, "/ready", StatusCode::OK, "ready").await;
    }

    #[tokio::test]
    async fn database_failure_affects_readiness_but_not_liveness() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        pool.close().await;
        let app = router(pool);
        check(app.clone(), "/health", StatusCode::OK, "healthy").await;
        check(app, "/ready", StatusCode::SERVICE_UNAVAILABLE, "not ready").await;
    }

    #[tokio::test]
    async fn readiness_times_out_when_pool_is_exhausted() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let _connection = pool.acquire().await.unwrap();
        tokio::time::pause();
        check(
            router(pool),
            "/ready",
            StatusCode::SERVICE_UNAVAILABLE,
            "not ready",
        )
        .await;
    }

    #[tokio::test]
    async fn probes_stay_at_root_with_a_prefixed_application() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let app = Router::new()
            .nest(
                "/jellyswarrm",
                Router::new().route("/", get(|| async { "app" })),
            )
            .fallback(|| async { axum::response::Redirect::temporary("/jellyswarrm") })
            .merge(router(pool));
        check(app.clone(), "/health", StatusCode::OK, "healthy").await;
        check(app, "/ready", StatusCode::OK, "ready").await;
    }
}
