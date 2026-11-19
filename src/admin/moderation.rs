//! Atomic, audited hide batches. Only the single realtime writer calls execute_hide.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, types::Json};
use uuid::Uuid;

use super::{
    AdminError,
    auth::{self, SessionProof},
};
use crate::domain::Bounds;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HideInput {
    operation_id: Uuid,
    stroke_ids: Vec<Uuid>,
    reason: String,
}
pub(crate) struct ValidatedHide {
    id: Uuid,
    ids: Vec<Uuid>,
    reason: String,
    digest: [u8; 32],
}

impl HideInput {
    pub fn validate(self) -> Result<ValidatedHide, AdminError> {
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
        hash.update(b"endlessdrawing/moderation/hide/v1\0");
        hash.update(payload);
        Ok(ValidatedHide {
            id: self.operation_id,
            ids,
            reason,
            digest: hash.finalize().into(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HideResult {
    pub operation_id: Uuid,
    pub hidden: Vec<Uuid>,
    pub already_hidden: Vec<Uuid>,
}

#[derive(FromRow)]
struct Row {
    id: Uuid,
    seq: i64,
    deleted: bool,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}
pub(crate) struct DeletedStroke {
    pub id: Uuid,
    pub seq: i64,
    pub bounds: Bounds,
}

pub(crate) async fn execute_hide(
    pool: &PgPool,
    proof: SessionProof,
    input: ValidatedHide,
) -> Result<(HideResult, Vec<DeletedStroke>), AdminError> {
    let mut transaction = pool.begin().await?;
    auth::authorize(&mut transaction, &proof).await?;
    let existing: Option<(Uuid, Vec<u8>, Json<HideResult>)> = sqlx::query_as(
        "SELECT actor_id, payload_digest, result FROM moderation_operations WHERE id = $1",
    )
    .bind(input.id)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some((actor, digest, result)) = existing {
        if actor != proof.account_id || digest.as_slice() != input.digest {
            return Err(AdminError::Conflict);
        }
        transaction.commit().await?;
        return Ok((result.0, Vec::new()));
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, seq, deleted_at IS NOT NULL AS deleted,
         (bounds[0])[0] AS max_x, (bounds[0])[1] AS max_y,
         (bounds[1])[0] AS min_x, (bounds[1])[1] AS min_y
         FROM strokes WHERE id = ANY($1) ORDER BY seq FOR UPDATE",
    )
    .bind(&input.ids)
    .fetch_all(&mut *transaction)
    .await?;
    if rows.len() != input.ids.len() {
        return Err(AdminError::StrokeNotFound);
    }
    let hidden: Vec<Uuid> = rows
        .iter()
        .filter(|row| !row.deleted)
        .map(|row| row.id)
        .collect();
    let already_hidden = rows
        .iter()
        .filter(|row| row.deleted)
        .map(|row| row.id)
        .collect();
    sqlx::query("UPDATE strokes SET deleted_at = clock_timestamp() WHERE id = ANY($1)")
        .bind(&hidden)
        .execute(&mut *transaction)
        .await?;
    let result = HideResult {
        operation_id: input.id,
        hidden,
        already_hidden,
    };
    sqlx::query("INSERT INTO moderation_operations(id, actor_id, payload_digest, reason, result) VALUES ($1, $2, $3, $4, $5)")
        .bind(input.id).bind(proof.account_id).bind(input.digest.as_slice()).bind(input.reason).bind(Json(&result))
        .execute(&mut *transaction).await?;
    sqlx::query(
        "INSERT INTO moderation_operation_strokes(operation_id, stroke_id, outcome)
         SELECT $1, id, CASE WHEN id = ANY($2) THEN 'hidden' ELSE 'already_hidden' END FROM strokes WHERE id = ANY($3)",
    ).bind(input.id).bind(&result.hidden).bind(&input.ids).execute(&mut *transaction).await?;
    // A hide also settles the report queue for these strokes (held ones stay hidden).
    super::reports::release_holds(&mut transaction, &input.ids, "hidden").await?;
    super::reports::close_reports(&mut transaction, &input.ids, "actioned").await?;
    auth::audit(&mut transaction, Some(proof.account_id), "strokes_hidden").await?;
    transaction
        .commit()
        .await
        .map_err(AdminError::commit_unknown)?;
    let events = rows
        .into_iter()
        .filter(|row| !row.deleted)
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

impl super::AdminService {
    pub(crate) async fn operation(
        &self,
        headers: &axum::http::HeaderMap,
        id: Uuid,
    ) -> Result<serde_json::Value, AdminError> {
        // Hide, dismiss and ban results share the operation table; their shapes differ.
        let (_, proof, _) = self.session(headers, false).await?;
        let result: Json<serde_json::Value> = sqlx::query_scalar(
            "SELECT result FROM moderation_operations WHERE id = $1 AND actor_id = $2",
        )
        .bind(id)
        .bind(proof.account_id)
        .fetch_optional(&self.0.pool)
        .await?
        .ok_or(AdminError::OperationNotFound)?;
        Ok(result.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hide_validation_is_bounded_and_normalized() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let id = Uuid::new_v4();
        let a = HideInput {
            operation_id: id,
            stroke_ids: vec![first, second],
            reason: " reason ".into(),
        }
        .validate()
        .unwrap();
        let b = HideInput {
            operation_id: id,
            stroke_ids: vec![second, first],
            reason: "reason".into(),
        }
        .validate()
        .unwrap();
        assert_eq!(a.digest, b.digest);
        for (ids, reason) in [
            (vec![], "reason".into()),
            (vec![first, first], "reason".into()),
            (vec![first], " ".into()),
            (vec![first], "a".repeat(1001)),
        ] {
            assert!(
                HideInput {
                    operation_id: id,
                    stroke_ids: ids,
                    reason
                }
                .validate()
                .is_err()
            );
        }
    }
}
