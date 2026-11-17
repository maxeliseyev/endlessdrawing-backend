//! Entry-point hints for new public sessions.
//!
//! A background task periodically samples live strokes (uniformly by `seq`,
//! through the unique `seq` index) and keeps the centers of their bounds in
//! memory. `hello.ack` offers one random anchor plus a small jitter, so new
//! visitors start next to existing drawing and spread out as the drawing
//! does: dense areas are proportionally more likely. An empty world (or a
//! failed sample) yields no hint and the client keeps its local default.
//! The hint is advisory: the camera never leaves the client.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use rand_core::{OsRng, RngCore};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::domain::WORLD_LIMIT;

/// Anchors kept per refresh.
pub(crate) const SAMPLE: usize = 64;
/// Uniform offset around an anchor in world units on each axis, so the
/// sampled stroke is nearby but not pinned to the screen center.
pub(crate) const JITTER: f64 = 400.0;

#[derive(Clone, Default)]
pub(crate) struct SpawnPoints {
    anchors: Arc<RwLock<Vec<[f64; 2]>>>,
}

impl SpawnPoints {
    pub(crate) fn pick(&self) -> Option<[f64; 2]> {
        let anchors = self.anchors.read().ok()?;
        let len = u64::try_from(anchors.len()).ok().filter(|len| *len > 0)?;
        let index = usize::try_from(OsRng.next_u64() % len).ok()?;
        Some(jitter(anchors[index], unit(), unit()))
    }

    pub(crate) fn replace(&self, anchors: Vec<[f64; 2]>) {
        if let Ok(mut current) = self.anchors.write() {
            *current = anchors;
        }
    }

    /// Refresh anchors now and then every `every` until `shutdown`.
    pub(crate) fn run(&self, pool: PgPool, every: Duration, shutdown: CancellationToken) {
        let points = self.clone();
        tokio::spawn(async move {
            loop {
                match sample(&pool).await {
                    Ok(anchors) => points.replace(anchors),
                    Err(error) => tracing::warn!(%error, "spawn sampling failed"),
                }
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(every) => {}
                }
            }
        });
    }
}

/// Uniform value in [0, 1).
fn unit() -> f64 {
    // 53 random mantissa bits; the cast is exact.
    #[allow(clippy::cast_precision_loss)]
    let value = (OsRng.next_u64() >> 11) as f64;
    value / (1u64 << 53) as f64
}

/// `u`, `v` in [0, 1) map to an offset in [-JITTER, JITTER) on each axis.
pub(crate) fn jitter(anchor: [f64; 2], u: f64, v: f64) -> [f64; 2] {
    let clamp = |value: f64| value.clamp(-WORLD_LIMIT, WORLD_LIMIT);
    [
        clamp(anchor[0] + (u * 2.0 - 1.0) * JITTER),
        clamp(anchor[1] + (v * 2.0 - 1.0) * JITTER),
    ]
}

async fn sample(pool: &PgPool) -> Result<Vec<[f64; 2]>, sqlx::Error> {
    let max: Option<i64> = sqlx::query_scalar("SELECT max(seq) FROM strokes")
        .fetch_one(pool)
        .await?;
    let Some(max) = max
        .and_then(|max| u64::try_from(max).ok())
        .filter(|max| *max > 0)
    else {
        return Ok(Vec::new());
    };
    let starts: Vec<i64> = (0..SAMPLE)
        .filter_map(|_| i64::try_from(1 + OsRng.next_u64() % max).ok())
        .collect();
    // First live stroke at or after each random seq; strokes right after a
    // long deleted run are slightly favoured, which is acceptable for a hint.
    let center =
        "((s.bounds[0])[0] + (s.bounds[1])[0]) / 2, ((s.bounds[0])[1] + (s.bounds[1])[1]) / 2";
    let mut rows: Vec<(f64, f64)> = sqlx::query_as(&format!(
        "SELECT {center} FROM unnest($1::bigint[]) AS r(start)
         CROSS JOIN LATERAL (SELECT bounds FROM strokes
             WHERE deleted_at IS NULL AND seq >= r.start ORDER BY seq LIMIT 1) s"
    ))
    .bind(&starts)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        // Every sampled start fell after the last live stroke.
        rows = sqlx::query_as(&format!(
            "SELECT {center} FROM (SELECT bounds FROM strokes
                 WHERE deleted_at IS NULL ORDER BY seq DESC LIMIT 1) s"
        ))
        .fetch_all(pool)
        .await?;
    }
    Ok(rows
        .into_iter()
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .map(|(x, y)| [x, y])
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_around_the_anchor_and_inside_the_world() {
        assert_eq!(jitter([100.0, -50.0], 0.5, 0.5), [100.0, -50.0]);
        assert_eq!(
            jitter([100.0, -50.0], 0.0, 0.0),
            [100.0 - JITTER, -50.0 - JITTER]
        );
        assert_eq!(
            jitter([WORLD_LIMIT, -WORLD_LIMIT], 0.999, 0.0),
            [WORLD_LIMIT, -WORLD_LIMIT]
        );
        for _ in 0..1000 {
            let u = unit();
            assert!((0.0..1.0).contains(&u));
        }
    }

    #[test]
    fn empty_world_has_no_hint_and_a_filled_one_picks_its_anchors() {
        let points = SpawnPoints::default();
        assert_eq!(points.pick(), None);
        points.replace(vec![[5000.0, 7000.0]]);
        for _ in 0..100 {
            let [x, y] = points.pick().unwrap();
            assert!((x - 5000.0).abs() <= JITTER && (y - 7000.0).abs() <= JITTER);
        }
    }
}
