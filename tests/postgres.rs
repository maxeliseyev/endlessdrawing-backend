//! Run explicitly against a disposable PostgreSQL instance; see README.md.

use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use endless_drawing_server::{
    app, db,
    domain::{Bounds, Brush, StrokeInput, StrokeLimits},
    session::OwnerDigest,
};
use serde_json::Value;
use sqlx::{PgPool, Row, types::Json};
use tower::ServiceExt;
use uuid::Uuid;

fn input() -> StrokeInput {
    StrokeInput {
        id: Uuid::new_v4(),
        points: vec![[0.0, 0.0], [10.0, 5.0]],
        brush: Brush::Round,
        width: 6.0,
        color: "#202020".into(),
        opacity: 0.8,
    }
}

// Test helper only. Production mutation APIs will use the sequential writer.
async fn insert(pool: &PgPool, input: StrokeInput, digest: &OwnerDigest) -> i64 {
    let validated = input.validate(StrokeLimits::default()).unwrap();
    let bounds = validated.bounds();
    let input = validated.input();
    let brush = match input.brush {
        Brush::Round => "round",
        Brush::Square => "square",
    };
    sqlx::query_scalar(
        "INSERT INTO strokes (id, owner_digest, points, brush, width, color, opacity, bounds)
         VALUES ($1, $2, $3, $4, $5, $6, $7, box(point($8, $9), point($10, $11)))
         RETURNING seq",
    )
    .bind(input.id)
    .bind(digest.as_bytes().as_slice())
    .bind(Json(&input.points))
    .bind(brush)
    .bind(input.width)
    .bind(&input.color)
    .bind(input.opacity)
    .bind(bounds.min_x)
    .bind(bounds.min_y)
    .bind(bounds.max_x)
    .bind(bounds.max_y)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn visible(pool: &PgPool, bounds: Bounds, after: i64, limit: i64) -> Vec<(Uuid, i64)> {
    sqlx::query_as(
        "SELECT id, seq FROM strokes
         WHERE deleted_at IS NULL
           AND bounds && box(point($1, $2), point($3, $4))
           AND seq > $5
         ORDER BY seq ASC LIMIT $6",
    )
    .bind(bounds.min_x)
    .bind(bounds.min_y)
    .bind(bounds.max_x)
    .bind(bounds.max_y)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL; run cargo test --test postgres -- --ignored"]
async fn migrations_are_repeatable_and_have_partial_indexes(pool: PgPool) {
    db::MIGRATOR.run(&pool).await.unwrap();
    db::MIGRATOR.run(&pool).await.unwrap();
    let indexes: Vec<String> =
        sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE tablename = 'strokes'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        indexes
            .iter()
            .any(|index| index.contains("USING gist (bounds)")
                && index.contains("deleted_at IS NULL"))
    );
    assert!(
        indexes
            .iter()
            .any(|index| index.contains("owner_digest, seq DESC")
                && index.contains("deleted_at IS NULL"))
    );
    let sequence_max: i64 = sqlx::query_scalar(
        "SELECT max_value FROM pg_sequences WHERE sequencename = 'strokes_seq_seq'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sequence_max, 9_007_199_254_740_991);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM strokes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL; run cargo test --test postgres -- --ignored"]
async fn roundtrips_vectors_bounds_and_ownership(pool: PgPool) {
    let owner = OwnerDigest::from_secret(&URL_SAFE_NO_PAD.encode([7; 32])).unwrap();
    let stranger = OwnerDigest::from_secret(&URL_SAFE_NO_PAD.encode([8; 32])).unwrap();
    let input = input();
    let id = input.id;
    let expected_points = input.points.clone();
    let expected_bounds = input
        .clone()
        .validate(StrokeLimits::default())
        .unwrap()
        .bounds();
    let seq = insert(&pool, input, &owner).await;
    assert!(seq > 0);

    // PostgreSQL box coordinates can be extracted via subscripts without a custom SQLx codec.
    let row = sqlx::query(
        "SELECT points, (bounds[0])[0] AS max_x, (bounds[0])[1] AS max_y,
                       (bounds[1])[0] AS min_x, (bounds[1])[1] AS min_y,
                owner_digest = $2 AS own, created_at IS NOT NULL AS has_time,
                deleted_at IS NULL AS visible
         FROM strokes WHERE id = $1",
    )
    .bind(id)
    .bind(owner.as_bytes().as_slice())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row.get::<Json<Vec<[f64; 2]>>, _>("points").0,
        expected_points
    );
    assert_eq!(row.get::<f64, _>("min_x"), expected_bounds.min_x);
    assert_eq!(row.get::<f64, _>("min_y"), expected_bounds.min_y);
    assert_eq!(row.get::<f64, _>("max_x"), expected_bounds.max_x);
    assert_eq!(row.get::<f64, _>("max_y"), expected_bounds.max_y);
    assert!(row.get::<bool, _>("own"));
    assert!(row.get::<bool, _>("has_time"));
    assert!(row.get::<bool, _>("visible"));
    let own: bool = sqlx::query_scalar("SELECT owner_digest = $2 FROM strokes WHERE id = $1")
        .bind(id)
        .bind(stranger.as_bytes().as_slice())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!own);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL; run cargo test --test postgres -- --ignored"]
async fn area_query_pages_and_excludes_soft_deleted_and_distant_strokes(pool: PgPool) {
    let owner = OwnerDigest::from_secret(&URL_SAFE_NO_PAD.encode([7; 32])).unwrap();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let input = input();
        ids.push((input.id, insert(&pool, input, &owner).await));
    }
    let mut distant = input();
    distant.points = vec![[1000.0, 1000.0]];
    insert(&pool, distant, &owner).await;
    let area = Bounds {
        min_x: -50.0,
        min_y: -50.0,
        max_x: 50.0,
        max_y: 50.0,
    };
    assert_eq!(visible(&pool, area, 0, 2).await, ids[..2]);
    assert_eq!(visible(&pool, area, ids[1].1, 2).await, ids[2..]);

    sqlx::query("UPDATE strokes SET deleted_at = now() WHERE id = $1")
        .bind(ids[1].0)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(visible(&pool, area, 0, 10).await, vec![ids[0], ids[2]]);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM strokes WHERE id = $1")
        .bind(ids[1].0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 1, "soft delete preserves the tombstone");
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL; run cargo test --test postgres -- --ignored"]
async fn thick_square_cap_intersects_area_beyond_point_extents(pool: PgPool) {
    let owner = OwnerDigest::from_secret(&URL_SAFE_NO_PAD.encode([7; 32])).unwrap();
    let mut input = input();
    input.brush = Brush::Square;
    input.width = 64.0;
    input.points = vec![[0.0, 0.0], [10.0, 10.0]];
    let id = input.id;
    let seq = insert(&pool, input, &owner).await;
    let area = Bounds {
        min_x: -46.0,
        min_y: -1.0,
        max_x: -44.0,
        max_y: 1.0,
    };
    assert_eq!(visible(&pool, area, 0, 10).await, vec![(id, seq)]);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL; run cargo test --test postgres -- --ignored"]
async fn database_constraints_reject_invalid_rows(pool: PgPool) {
    for (points, width, opacity, color, digest) in [
        (serde_json::json!([]), 1.0, 1.0, "#000000", vec![0_u8; 32]),
        (serde_json::json!({}), 1.0, 1.0, "#000000", vec![0_u8; 32]),
        (
            serde_json::json!([[0, 0]]),
            f64::NAN,
            1.0,
            "#000000",
            vec![0_u8; 32],
        ),
        (
            serde_json::json!([[0, 0]]),
            65.0,
            1.0,
            "#000000",
            vec![0_u8; 32],
        ),
        (
            serde_json::json!([[0, 0]]),
            1.0,
            0.0,
            "#000000",
            vec![0_u8; 32],
        ),
        (
            serde_json::json!([[0, 0]]),
            1.0,
            f64::INFINITY,
            "#000000",
            vec![0_u8; 32],
        ),
        (
            serde_json::json!([[0, 0]]),
            1.0,
            f64::NAN,
            "#000000",
            vec![0_u8; 32],
        ),
        (
            serde_json::json!([[0, 0]]),
            1.0,
            1.0,
            "#GG0000",
            vec![0_u8; 32],
        ),
        (
            serde_json::json!([[0, 0]]),
            1.0,
            1.0,
            "#000000",
            vec![0_u8; 31],
        ),
    ] {
        let result = sqlx::query(
            "INSERT INTO strokes (id, owner_digest, points, brush, width, color, opacity, bounds)
             VALUES ($1, $2, $3, 'round', $4, $5, $6, box(point(0, 0), point(1, 1)))",
        )
        .bind(Uuid::new_v4())
        .bind(digest)
        .bind(Json::<Value>(points))
        .bind(width)
        .bind(color)
        .bind(opacity)
        .execute(&pool)
        .await;
        assert!(result.is_err());
    }
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL; run cargo test --test postgres -- --ignored"]
async fn readiness_tracks_database_and_migrated_schema(pool: PgPool) {
    let application = app::router(pool.clone(), Duration::from_secs(1), None);
    let response = application
        .clone()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Make the expected relation unavailable without breaking moderation foreign keys.
    sqlx::query("ALTER TABLE strokes RENAME TO test_unavailable_strokes")
        .execute(&pool)
        .await
        .unwrap();
    let response = application
        .clone()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    pool.close().await;
    let response = application
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
