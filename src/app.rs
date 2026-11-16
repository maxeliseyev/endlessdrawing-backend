//! HTTP routes. WebSocket transport is the next implementation stage.

use std::{path::PathBuf, time::Duration};

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Serialize;
use sqlx::PgPool;
use tower_http::services::ServeDir;

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    query_timeout: Duration,
}

#[derive(Serialize)]
struct Probe {
    status: &'static str,
}

/// Builds health/readiness routes and optionally serves a Vite production build.
///
/// Unknown routes return 404; no placeholder WS or HTTP mutation API is advertised.
pub fn router(pool: PgPool, query_timeout: Duration, static_dir: Option<PathBuf>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .with_state(AppState {
            pool,
            query_timeout,
        });
    if let Some(path) = static_dir {
        router = router.fallback_service(ServeDir::new(path));
    }
    router
}

async fn health() -> Json<Probe> {
    Json(Probe { status: "ok" })
}

async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Probe>) {
    // LIMIT 0 checks connectivity and the migrated relation, without reading strokes.
    let result = tokio::time::timeout(
        state.query_timeout,
        sqlx::query("SELECT id, seq, points, bounds, deleted_at FROM strokes LIMIT 0")
            .execute(&state.pool),
    )
    .await;
    match result {
        Ok(Ok(_)) => (StatusCode::OK, Json(Probe { status: "ready" })),
        // A public probe must not disclose a DSN, SQL error or driver internals.
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Probe {
                status: "not_ready",
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    use super::*;

    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://canvas:secret@127.0.0.1:1/canvas")
            .unwrap()
    }

    #[tokio::test]
    async fn health_does_not_require_a_database() {
        let pool = lazy_pool();
        pool.close().await;
        let response = router(pool, Duration::from_millis(50), None)
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            r#"{"status":"ok"}"#
        );
    }

    #[tokio::test]
    async fn readiness_fails_safely_when_database_is_unavailable() {
        let pool = lazy_pool();
        pool.close().await;
        let response = router(pool, Duration::from_millis(50), None)
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, r#"{"status":"not_ready"}"#);
    }

    #[tokio::test]
    async fn optional_static_serving_does_not_mask_unknown_routes() {
        let directory = tempfile::tempdir().unwrap();
        tokio::fs::write(directory.path().join("index.html"), "<html>canvas</html>")
            .await
            .unwrap();
        let application = router(
            lazy_pool(),
            Duration::from_millis(50),
            Some(directory.path().into()),
        );
        let response = application
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "<html>canvas</html>"
        );
        for path in ["/ws", "/api/v1/strokes", "/unknown-page"] {
            let response = application
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }
}
