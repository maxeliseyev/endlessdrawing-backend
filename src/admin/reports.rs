//! Report queue, dismissals and author bans (docs/admin-api.md §5, docs/protocol-v5-safety.md).
//! Mutations run only in the single realtime writer, like hide.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgConnection, PgPool, types::Json};
use uuid::Uuid;

use super::{
    AdminError,
    auth::{self, SessionProof},
    moderation::DeletedStroke,
};
use crate::{domain::Bounds, storage::StoredStroke};

/// One reported stroke in the open queue, aggregated over its open reports.
#[derive(Serialize, FromRow)]
pub(crate) struct QueueItem {
    stroke_id: Uuid,
    reports: i64,
    reporters: i64,
    reasons: Vec<String>,
    block_requests: i64,
    first_at: String,
    last_at: String,
    /// Hidden by the report threshold, awaiting a decision.
    held: bool,
    /// Still publicly visible (not hidden, held or deleted by its author).
    visible: bool,
    x: f64,
    y: f64,
}

#[derive(Serialize)]
pub(crate) struct Queue {
    items: Vec<QueueItem>,
    open_reports: i64,
}

const QUEUE_LIMIT: i64 = 200;

pub(crate) async fn queue(pool: &PgPool) -> Result<Queue, AdminError> {
    let items = sqlx::query_as::<_, QueueItem>(
        "SELECT s.id AS stroke_id,
                count(*) AS reports,
                count(DISTINCT r.reporter_digest) AS reporters,
                array_agg(DISTINCT r.reason ORDER BY r.reason) AS reasons,
                count(*) FILTER (WHERE r.block_author) AS block_requests,
                to_char(min(r.created_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS first_at,
                to_char(max(r.created_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS last_at,
                EXISTS (SELECT 1 FROM stroke_holds h WHERE h.stroke_id = s.id AND h.released_at IS NULL) AS held,
                s.deleted_at IS NULL AS visible,
                ((s.bounds[0])[0] + (s.bounds[1])[0]) / 2 AS x,
                ((s.bounds[0])[1] + (s.bounds[1])[1]) / 2 AS y
         FROM stroke_report_items i
         JOIN stroke_reports r ON r.id = i.report_id AND r.status = 'open'
         JOIN strokes s ON s.id = i.stroke_id
         GROUP BY s.id
         ORDER BY held DESC, max(r.created_at) DESC
         LIMIT $1",
    )
    .bind(QUEUE_LIMIT)
    .fetch_all(pool)
    .await?;
    let open_reports =
        sqlx::query_scalar("SELECT count(*) FROM stroke_reports WHERE status = 'open'")
            .fetch_one(pool)
            .await?;
    Ok(Queue {
        items,
        open_reports,
    })
}

/// Dismiss or ban request: same bounds and idempotency rules as hide.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecisionInput {
    operation_id: Uuid,
    stroke_ids: Vec<Uuid>,
    reason: String,
}

pub(crate) struct Decision {
    id: Uuid,
    ids: Vec<Uuid>,
    reason: String,
    digest: [u8; 32],
}

#[derive(Clone, Copy)]
pub(crate) enum DecisionKind {
    Dismiss,
    Ban,
}

impl DecisionInput {
    pub fn validate(self, kind: DecisionKind) -> Result<Decision, AdminError> {
        if self.stroke_ids.is_empty() || self.stroke_ids.len() > 100 {
            return Err(AdminError::Invalid);
        }
        let mut ids = self.stroke_ids;
        ids.sort_unstable();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(AdminError::Invalid);
        }
        let reason = self.reason.trim().to_owned();
        if reason.is_empty() || reason.chars().count() > 1000 || reason.len() > 4000 {
            return Err(AdminError::Invalid);
        }
        let payload = serde_json::to_vec(&(&ids, &reason)).map_err(|_| AdminError::Unavailable)?;
        let mut hash = Sha256::new();
        hash.update(match kind {
            DecisionKind::Dismiss => b"endlessdrawing/moderation/dismiss/v1\0".as_slice(),
            DecisionKind::Ban => b"endlessdrawing/moderation/ban/v1\0".as_slice(),
        });
        hash.update(payload);
        Ok(Decision {
            id: self.operation_id,
            ids,
            reason,
            digest: hash.finalize().into(),
        })
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct DismissResult {
    pub operation_id: Uuid,
    /// Strokes held by reports and now public again.
    pub restored: Vec<Uuid>,
    pub dismissed_reports: i64,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct BanResult {
    pub operation_id: Uuid,
    pub banned_authors: i64,
    /// Every stroke of the banned authors that this operation hid.
    pub hidden: Vec<Uuid>,
}

/// A repeated operation id returns the stored result (no second broadcast); a different
/// actor or payload under the same id is a conflict.
async fn replay<T: for<'de> Deserialize<'de>>(
    connection: &mut PgConnection,
    proof: &SessionProof,
    decision: &Decision,
) -> Result<Option<T>, AdminError> {
    let existing: Option<(Uuid, Vec<u8>, Json<serde_json::Value>)> = sqlx::query_as(
        "SELECT actor_id, payload_digest, result FROM moderation_operations WHERE id = $1",
    )
    .bind(decision.id)
    .fetch_optional(connection)
    .await?;
    let Some((actor, digest, result)) = existing else {
        return Ok(None);
    };
    if actor != proof.account_id || digest.as_slice() != decision.digest {
        return Err(AdminError::Conflict);
    }
    serde_json::from_value(result.0)
        .map(Some)
        .map_err(|_| AdminError::Conflict)
}

async fn record<T: Serialize>(
    connection: &mut PgConnection,
    proof: &SessionProof,
    decision: &Decision,
    result: &T,
) -> Result<(), AdminError> {
    sqlx::query("INSERT INTO moderation_operations(id, actor_id, payload_digest, reason, result) VALUES ($1, $2, $3, $4, $5)")
        .bind(decision.id)
        .bind(proof.account_id)
        .bind(decision.digest.as_slice())
        .bind(&decision.reason)
        .bind(Json(result))
        .execute(connection)
        .await?;
    Ok(())
}

async fn lock_existing(connection: &mut PgConnection, ids: &[Uuid]) -> Result<(), AdminError> {
    let found: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM strokes WHERE id = ANY($1) ORDER BY seq FOR UPDATE")
            .bind(ids)
            .fetch_all(connection)
            .await?;
    if found.len() != ids.len() {
        return Err(AdminError::StrokeNotFound);
    }
    Ok(())
}

/// Closes the open reports that mention any of `ids` with `status`; returns how many.
pub(crate) async fn close_reports(
    connection: &mut PgConnection,
    ids: &[Uuid],
    status: &str,
) -> Result<i64, sqlx::Error> {
    let closed = sqlx::query(
        "UPDATE stroke_reports SET status = $2, decided_at = now()
         WHERE status = 'open' AND id IN (SELECT report_id FROM stroke_report_items WHERE stroke_id = ANY($1))",
    )
    .bind(ids)
    .bind(status)
    .execute(connection)
    .await?
    .rows_affected();
    Ok(i64::try_from(closed).unwrap_or(i64::MAX))
}

/// Releases unresolved holds of `ids` with `outcome` (`hidden` or `restored`); returns them.
pub(crate) async fn release_holds(
    connection: &mut PgConnection,
    ids: &[Uuid],
    outcome: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE stroke_holds SET released_at = now(), outcome = $2
         WHERE stroke_id = ANY($1) AND released_at IS NULL RETURNING stroke_id",
    )
    .bind(ids)
    .bind(outcome)
    .fetch_all(connection)
    .await
}

/// Dismisses the open reports of the strokes and publishes again those that were held.
pub(crate) async fn execute_dismiss(
    pool: &PgPool,
    proof: SessionProof,
    decision: Decision,
) -> Result<(DismissResult, Vec<Arc<StoredStroke>>), AdminError> {
    let mut transaction = pool.begin().await?;
    auth::authorize(&mut transaction, &proof).await?;
    if let Some(result) = replay(&mut transaction, &proof, &decision).await? {
        transaction.commit().await?;
        return Ok((result, Vec::new()));
    }
    lock_existing(&mut transaction, &decision.ids).await?;
    let restored = release_holds(&mut transaction, &decision.ids, "restored").await?;
    if !restored.is_empty() {
        sqlx::query("UPDATE strokes SET deleted_at = NULL WHERE id = ANY($1)")
            .bind(&restored)
            .execute(&mut *transaction)
            .await?;
    }
    let dismissed_reports = close_reports(&mut transaction, &decision.ids, "dismissed").await?;
    let records = crate::storage::load(&mut transaction, &restored).await?;
    let result = DismissResult {
        operation_id: decision.id,
        restored,
        dismissed_reports,
    };
    record(&mut transaction, &proof, &decision, &result).await?;
    auth::audit(
        &mut transaction,
        Some(proof.account_id),
        "reports_dismissed",
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(AdminError::commit_unknown)?;
    Ok((result, records.into_iter().map(Arc::new).collect()))
}

#[derive(FromRow)]
struct HiddenRow {
    id: Uuid,
    seq: i64,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

/// Bans the authors of the strokes: records their digests and hides all their visible strokes.
pub(crate) async fn execute_ban(
    pool: &PgPool,
    proof: SessionProof,
    decision: Decision,
) -> Result<(BanResult, Vec<DeletedStroke>), AdminError> {
    let mut transaction = pool.begin().await?;
    auth::authorize(&mut transaction, &proof).await?;
    if let Some(result) = replay(&mut transaction, &proof, &decision).await? {
        transaction.commit().await?;
        return Ok((result, Vec::new()));
    }
    lock_existing(&mut transaction, &decision.ids).await?;
    let owners: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT DISTINCT owner_digest FROM strokes WHERE id = ANY($1)")
            .bind(&decision.ids)
            .fetch_all(&mut *transaction)
            .await?;
    sqlx::query("INSERT INTO banned_owners (owner_digest) SELECT unnest($1::bytea[]) ON CONFLICT DO NOTHING")
        .bind(&owners)
        .execute(&mut *transaction)
        .await?;
    let rows = sqlx::query_as::<_, HiddenRow>(
        "UPDATE strokes SET deleted_at = clock_timestamp()
         WHERE owner_digest = ANY($1) AND deleted_at IS NULL
         RETURNING id, seq, (bounds[0])[0] AS max_x, (bounds[0])[1] AS max_y,
                   (bounds[1])[0] AS min_x, (bounds[1])[1] AS min_y",
    )
    .bind(&owners)
    .fetch_all(&mut *transaction)
    .await?;
    let all: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM strokes WHERE owner_digest = ANY($1)")
        .bind(&owners)
        .fetch_all(&mut *transaction)
        .await?;
    release_holds(&mut transaction, &all, "hidden").await?;
    close_reports(&mut transaction, &all, "actioned").await?;
    let mut hidden: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    hidden.sort_unstable();
    let result = BanResult {
        operation_id: decision.id,
        banned_authors: i64::try_from(owners.len()).unwrap_or(i64::MAX),
        hidden,
    };
    record(&mut transaction, &proof, &decision, &result).await?;
    sqlx::query(
        "INSERT INTO moderation_operation_strokes(operation_id, stroke_id, outcome) SELECT $1, unnest($2::uuid[]), 'hidden'",
    )
    .bind(decision.id)
    .bind(&result.hidden)
    .execute(&mut *transaction)
    .await?;
    auth::audit(&mut transaction, Some(proof.account_id), "authors_banned").await?;
    transaction
        .commit()
        .await
        .map_err(AdminError::commit_unknown)?;
    let events = rows
        .into_iter()
        .map(|row| DeletedStroke {
            id: row.id,
            seq: row.seq,
            bounds: Bounds {
                min_x: row.min_x,
                min_y: row.min_y,
                max_x: row.max_x,
                max_y: row.max_y,
            },
        })
        .collect();
    Ok((result, events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_are_bounded_normalized_and_kind_specific() {
        let (first, second, id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let input = |ids: Vec<Uuid>, reason: &str| DecisionInput {
            operation_id: id,
            stroke_ids: ids,
            reason: reason.into(),
        };
        let a = input(vec![first, second], " spam ")
            .validate(DecisionKind::Ban)
            .unwrap();
        let b = input(vec![second, first], "spam")
            .validate(DecisionKind::Ban)
            .unwrap();
        let c = input(vec![first, second], "spam")
            .validate(DecisionKind::Dismiss)
            .unwrap();
        assert_eq!(a.digest, b.digest);
        assert_ne!(
            a.digest, c.digest,
            "the same payload as a different action conflicts"
        );
        assert!(input(vec![], "x").validate(DecisionKind::Ban).is_err());
        assert!(
            input(vec![first, first], "x")
                .validate(DecisionKind::Dismiss)
                .is_err()
        );
        assert!(
            input(vec![first], "  ")
                .validate(DecisionKind::Dismiss)
                .is_err()
        );
    }
}
