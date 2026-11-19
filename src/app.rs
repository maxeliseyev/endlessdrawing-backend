//! HTTP routes. WebSocket transport is the next implementation stage.

use std::{path::PathBuf, time::Duration};

use std::collections::BTreeMap;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
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
        .route("/internal/moderation/summary", get(moderation_summary))
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

/// Counts only, for the VPS monitor's Telegram notice (docs/protocol-v5-safety.md).
#[derive(Serialize)]
struct ModerationSummary {
    open_reports: i64,
    held: i64,
    latest_report_seq: i64,
    oldest_open_age_s: i64,
    open_by_reason: BTreeMap<String, i64>,
}

/// Any proxy-added header means the request came through Caddy, i.e. from the internet:
/// the summary is only for direct requests to the published loopback port.
fn proxied(headers: &HeaderMap) -> bool {
    [
        "x-forwarded-for",
        "x-forwarded-proto",
        "x-forwarded-host",
        "forwarded",
        "via",
    ]
    .iter()
    .any(|name| headers.contains_key(*name))
}

async fn moderation_summary(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if proxied(&headers) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let query = async {
        let (open_reports, held, latest_report_seq, oldest_open_age_s): (i64, i64, i64, i64) =
            sqlx::query_as(
                "SELECT (SELECT count(*) FROM stroke_reports WHERE status = 'open'),
                        (SELECT count(*) FROM stroke_holds WHERE released_at IS NULL),
                        (SELECT coalesce(max(seq), 0) FROM stroke_reports),
                        (SELECT coalesce(floor(extract(epoch FROM now() - min(created_at)))::bigint, 0)
                           FROM stroke_reports WHERE status = 'open')",
            )
            .fetch_one(&state.pool)
            .await?;
        let reasons: Vec<(String, i64)> = sqlx::query_as(
            "SELECT reason, count(*) FROM stroke_reports WHERE status = 'open' GROUP BY reason",
        )
        .fetch_all(&state.pool)
        .await?;
        Ok::<_, sqlx::Error>(ModerationSummary {
            open_reports,
            held,
            latest_report_seq,
            oldest_open_age_s,
            open_by_reason: reasons.into_iter().collect(),
        })
    };
    match tokio::time::timeout(state.query_timeout, query).await {
        Ok(Ok(summary)) => Json(summary).into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
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
    async fn moderation_summary_is_not_reachable_through_the_proxy() {
        let router = router(lazy_pool(), Duration::from_millis(50), None);
        for header in ["x-forwarded-for", "forwarded", "via"] {
            let response = router
                .clone()
                .oneshot(
                    Request::get("/internal/moderation/summary")
                        .header(header, "203.0.113.7")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{header}");
        }
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
