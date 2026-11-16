use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio_util::task::AbortOnDropHandle;

use super::*;
use crate::domain::{Brush, StrokeInput};

fn owner() -> OwnerDigest {
    OwnerDigest::from_secret(&URL_SAFE_NO_PAD.encode([42; 32])).unwrap()
}

fn stroke() -> ValidatedStroke {
    StrokeInput {
        id: Uuid::new_v4(),
        points: vec![[0.0, 0.0]],
        brush: Brush::Round,
        width: 6.0,
        color: "#202020".into(),
        opacity: 1.0,
    }
    .validate(StrokeLimits::default())
    .unwrap()
}

fn area() -> Bounds {
    Bounds {
        min_x: -100.0,
        min_y: -100.0,
        max_x: 100.0,
        max_y: 100.0,
    }
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn snapshot_barrier_separates_old_rows_from_buffered_mutations(pool: PgPool) {
    let (service, worker) =
        Realtime::start(pool, StrokeLimits::default(), RealtimeSettings::default()).unwrap();
    let mut worker = AbortOnDropHandle::new(worker);
    let first = stroke();
    let first_id = first.input().id;
    assert!(service.writer.create(first, owner()).await.is_ok());
    let mut snapshot = service.writer.snapshot().await.ok().unwrap();
    let second = stroke();
    let second_id = second.input().id;
    assert!(service.writer.create(second.clone(), owner()).await.is_ok());
    assert!(service.writer.delete(first_id, owner()).await.is_ok());
    let rows = storage::page(&mut snapshot.transaction, area(), 0, 100)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].input.id, first_id,
        "the snapshot retains a row deleted after its boundary"
    );
    let event = snapshot.events.recv().await.unwrap();
    let Event::Created(created) = event.as_ref() else {
        panic!("expected create");
    };
    assert_eq!(created.input.id, second_id);
    assert!(created.seq > rows[0].seq);
    let event = snapshot.events.recv().await.unwrap();
    assert!(matches!(event.as_ref(), Event::Deleted { id, .. } if *id == first_id));
    assert!(service.writer.create(second, owner()).await.is_ok());
    assert!(service.writer.delete(first_id, owner()).await.is_ok());
    assert!(
        matches!(
            snapshot.events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ),
        "retries do not publish duplicate events"
    );
    snapshot.transaction.rollback().await.unwrap();
    service.shutdown.cancel();
    (&mut worker).await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn event_buffer_overflow_is_detected_and_fresh_snapshot_recovers(pool: PgPool) {
    let settings = RealtimeSettings {
        event_buffer_capacity: 2,
        ..Default::default()
    };
    let (service, worker) = Realtime::start(pool, StrokeLimits::default(), settings).unwrap();
    let mut worker = AbortOnDropHandle::new(worker);
    let mut snapshot = service.writer.snapshot().await.ok().unwrap();
    for _ in 0..4 {
        assert!(service.writer.create(stroke(), owner()).await.is_ok());
    }
    assert!(matches!(
        snapshot.events.recv().await,
        Err(broadcast::error::RecvError::Lagged(_))
    ));
    assert!(
        storage::page(&mut snapshot.transaction, area(), 0, 100)
            .await
            .unwrap()
            .is_empty()
    );
    snapshot.transaction.rollback().await.unwrap();
    let mut fresh = service.writer.snapshot().await.ok().unwrap();
    assert_eq!(
        storage::page(&mut fresh.transaction, area(), 0, 100)
            .await
            .unwrap()
            .len(),
        4
    );
    fresh.transaction.rollback().await.unwrap();
    service.shutdown.cancel();
    (&mut worker).await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn failed_transaction_has_no_ack_or_event_and_writer_continues(pool: PgPool) {
    sqlx::query("ALTER TABLE strokes ADD CONSTRAINT test_reject_color CHECK (color <> '#FF0000')")
        .execute(&pool)
        .await
        .unwrap();
    let (service, worker) = Realtime::start(
        pool.clone(),
        StrokeLimits::default(),
        RealtimeSettings::default(),
    )
    .unwrap();
    let mut worker = AbortOnDropHandle::new(worker);
    let mut snapshot = service.writer.snapshot().await.ok().unwrap();
    let mut invalid = stroke().input().clone();
    invalid.color = "#FF0000".into();
    let result = service
        .writer
        .create(invalid.validate(StrokeLimits::default()).unwrap(), owner())
        .await;
    assert!(matches!(result, Err(ErrorCode::InternalError)));
    let valid = stroke();
    let id = valid.input().id;
    assert!(service.writer.create(valid, owner()).await.is_ok());
    let event = snapshot.events.recv().await.unwrap();
    assert!(matches!(event.as_ref(), Event::Created(record) if record.input.id == id));
    assert!(matches!(
        snapshot.events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM strokes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    snapshot.transaction.rollback().await.unwrap();
    service.shutdown.cancel();
    (&mut worker).await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn parallel_requests_commit_and_publish_in_increasing_creation_order(pool: PgPool) {
    let (service, worker) =
        Realtime::start(pool, StrokeLimits::default(), RealtimeSettings::default()).unwrap();
    let mut worker = AbortOnDropHandle::new(worker);
    let mut snapshot = service.writer.snapshot().await.ok().unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let writer = service.writer.clone();
        tasks.spawn(async move { writer.create(stroke(), owner()).await });
    }
    while let Some(result) = tasks.join_next().await {
        assert!(result.unwrap().is_ok());
    }
    let mut previous = 0;
    for _ in 0..32 {
        let event = snapshot.events.recv().await.unwrap();
        let Event::Created(record) = event.as_ref() else {
            panic!("expected create");
        };
        assert!(record.seq > previous);
        previous = record.seq;
    }
    snapshot.transaction.rollback().await.unwrap();
    service.shutdown.cancel();
    (&mut worker).await.unwrap();
}
