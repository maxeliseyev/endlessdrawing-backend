//! Parameterized storage operations. Mutations run only in the sequential writer;
//! snapshot membership and immutable geometry reads do not mutate records.

use sqlx::{FromRow, PgConnection, types::Json};
use uuid::Uuid;

use crate::{
    codec::Profile,
    domain::{Bounds, Brush, StrokeInput, ValidatedStroke},
    protocol::{ErrorCode, PublicStroke, ReportReason},
    session::OwnerDigest,
};

const COLUMNS: &str = "id, seq, owner_digest, points, brush, width, color, opacity,
    (bounds[0])[0] AS max_x, (bounds[0])[1] AS max_y,
    (bounds[1])[0] AS min_x, (bounds[1])[1] AS min_y,
    deleted_at IS NOT NULL AS deleted";

#[derive(FromRow)]
struct StrokeRow {
    id: Uuid,
    seq: i64,
    owner_digest: Vec<u8>,
    points: Json<Vec<[f64; 2]>>,
    brush: String,
    width: f64,
    color: String,
    opacity: f64,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    deleted: bool,
}

// Intentionally neither Serialize nor Debug: includes private ownership data.
pub(crate) struct StoredStroke {
    pub input: StrokeInput,
    pub seq: i64,
    pub bounds: Bounds,
    owner: Vec<u8>,
    deleted: bool,
}

impl StoredStroke {
    /// The stroke as `owner` sees it in `profile`: v5+ adds the per-viewer author
    /// tag, v6 sends quantized `path` geometry.
    pub fn public(&self, owner: &OwnerDigest, profile: Profile) -> PublicStroke {
        PublicStroke {
            input: self.input.clone(),
            seq: self.seq,
            own: self.is_owned_by(owner),
            author: profile.safety().then(|| owner.author_tag(&self.owner)),
            compact: profile.compact(),
        }
    }

    fn is_owned_by(&self, owner: &OwnerDigest) -> bool {
        self.owner.as_slice() == owner.as_bytes()
    }
}

impl TryFrom<StrokeRow> for StoredStroke {
    type Error = sqlx::Error;

    fn try_from(row: StrokeRow) -> Result<Self, Self::Error> {
        let Some(brush) = Brush::from_stored(&row.brush) else {
            return Err(sqlx::Error::Decode(
                std::io::Error::other("invalid stored brush").into(),
            ));
        };
        Ok(Self {
            input: StrokeInput {
                id: row.id,
                points: row.points.0,
                brush,
                width: row.width,
                color: row.color,
                opacity: row.opacity,
            },
            seq: row.seq,
            bounds: Bounds {
                min_x: row.min_x,
                min_y: row.min_y,
                max_x: row.max_x,
                max_y: row.max_y,
            },
            owner: row.owner_digest,
            deleted: row.deleted,
        })
    }
}

pub(crate) enum StorageError {
    Rejected(ErrorCode),
    Database(sqlx::Error),
    CommitUnknown(sqlx::Error),
}

impl From<sqlx::Error> for StorageError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

async fn find(
    connection: &mut PgConnection,
    id: Uuid,
) -> Result<Option<StoredStroke>, sqlx::Error> {
    let query = format!("SELECT {COLUMNS} FROM strokes WHERE id = $1");
    sqlx::query_as::<_, StrokeRow>(&query)
        .bind(id)
        .fetch_optional(connection)
        .await?
        .map(TryInto::try_into)
        .transpose()
}

/// Returns the canonical record and whether a new row was inserted.
pub(crate) async fn create(
    connection: &mut PgConnection,
    stroke: ValidatedStroke,
    owner: &OwnerDigest,
) -> Result<(StoredStroke, bool), StorageError> {
    let input = stroke.input();
    if banned(connection, owner).await? {
        return Err(StorageError::Rejected(ErrorCode::Banned));
    }
    if let Some(existing) = find(connection, input.id).await? {
        if !existing.is_owned_by(owner) || existing.input != *input {
            return Err(StorageError::Rejected(ErrorCode::IdConflict));
        }
        if existing.deleted {
            return Err(StorageError::Rejected(ErrorCode::StrokeDeleted));
        }
        return Ok((existing, false));
    }
    let bounds = stroke.bounds();
    let brush = input.brush.as_str();
    let query = format!(
        "INSERT INTO strokes (id, owner_digest, points, brush, width, color, opacity, bounds)
         VALUES ($1, $2, $3, $4, $5, $6, $7, box(point($8, $9), point($10, $11)))
         RETURNING {COLUMNS}"
    );
    let row = sqlx::query_as::<_, StrokeRow>(&query)
        .bind(input.id)
        .bind(owner.as_bytes().as_slice())
        .bind(Json(&input.points))
        .bind(brush)
        .bind(input.width)
        .bind(&input.color)
        .bind(input.opacity)
        .bind(bounds.min_x)
        .bind(bounds.min_y)
        .bind(bounds.max_x)
        .bind(bounds.max_y)
        .fetch_one(connection)
        .await?;
    Ok((row.try_into()?, true))
}

/// Returns the original record and whether it was newly soft-deleted.
pub(crate) async fn delete(
    connection: &mut PgConnection,
    id: Uuid,
    owner: &OwnerDigest,
) -> Result<(StoredStroke, bool), StorageError> {
    let existing = find(connection, id)
        .await?
        .ok_or(StorageError::Rejected(ErrorCode::NotFound))?;
    if !existing.is_owned_by(owner) {
        return Err(StorageError::Rejected(ErrorCode::Forbidden));
    }
    let changed = !existing.deleted;
    if changed {
        sqlx::query("UPDATE strokes SET deleted_at = now() WHERE id = $1")
            .bind(id)
            .execute(connection)
            .await?;
    }
    Ok((existing, changed))
}

/// Current records by id (any state), for publishing restored strokes.
pub(crate) async fn load(
    connection: &mut PgConnection,
    ids: &[Uuid],
) -> Result<Vec<StoredStroke>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let query = format!("SELECT {COLUMNS} FROM strokes WHERE id = ANY($1) ORDER BY seq");
    sqlx::query_as::<_, StrokeRow>(&query)
        .bind(ids)
        .fetch_all(connection)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
}

async fn banned(connection: &mut PgConnection, owner: &OwnerDigest) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM banned_owners WHERE owner_digest = $1)")
        .bind(owner.as_bytes().as_slice())
        .fetch_one(connection)
        .await
}

/// v5 report limits (docs/protocol-v5-safety.md).
pub(crate) struct ReportPolicy {
    pub per_hour: i64,
    pub hold_threshold: i64,
}

/// Stores one report and holds every reported stroke that now has `hold_threshold`
/// distinct open reporters. Returns the report id and the newly held strokes (for
/// public `stroke.deleted`). The caller commits.
pub(crate) async fn report(
    connection: &mut PgConnection,
    reporter: &OwnerDigest,
    ids: &[Uuid],
    reason: ReportReason,
    block_author: bool,
    policy: &ReportPolicy,
) -> Result<(Uuid, Vec<StoredStroke>), StorageError> {
    let reporter_bytes = reporter.as_bytes().as_slice();
    if banned(connection, reporter).await? {
        return Err(StorageError::Rejected(ErrorCode::Banned));
    }
    // Serializes one reporter's concurrent reports so the hourly limit is exact.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(encode($1, 'hex'), 5))")
        .bind(reporter_bytes)
        .execute(&mut *connection)
        .await?;
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM stroke_reports WHERE reporter_digest = $1 AND created_at > now() - interval '1 hour'",
    )
    .bind(reporter_bytes)
    .fetch_one(&mut *connection)
    .await?;
    if recent >= policy.per_hour {
        return Err(StorageError::Rejected(ErrorCode::RateLimited));
    }
    // Lock the reported rows: a concurrent hide/delete/hold cannot interleave.
    let query = format!("SELECT {COLUMNS} FROM strokes WHERE id = ANY($1) ORDER BY id FOR UPDATE");
    let strokes: Vec<StoredStroke> = sqlx::query_as::<_, StrokeRow>(&query)
        .bind(ids)
        .fetch_all(&mut *connection)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect::<Result<_, _>>()?;
    if strokes.len() != ids.len() || strokes.iter().any(|stroke| stroke.deleted) {
        return Err(StorageError::Rejected(ErrorCode::NotFound));
    }
    if strokes.iter().any(|stroke| stroke.is_owned_by(reporter)) {
        return Err(StorageError::Rejected(ErrorCode::ValidationFailed));
    }
    let report_id = Uuid::new_v4();
    sqlx::query("INSERT INTO stroke_reports (id, reporter_digest, reason, block_author) VALUES ($1, $2, $3, $4)")
        .bind(report_id)
        .bind(reporter_bytes)
        .bind(reason.as_str())
        .bind(block_author)
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "INSERT INTO stroke_report_items (report_id, stroke_id) SELECT $1, unnest($2::uuid[])",
    )
    .bind(report_id)
    .bind(ids)
    .execute(&mut *connection)
    .await?;
    let reached: Vec<Uuid> = sqlx::query_scalar(
        "SELECT i.stroke_id FROM stroke_report_items i JOIN stroke_reports r ON r.id = i.report_id
         WHERE i.stroke_id = ANY($1) AND r.status = 'open'
         GROUP BY i.stroke_id HAVING count(DISTINCT r.reporter_digest) >= $2",
    )
    .bind(ids)
    .bind(policy.hold_threshold)
    .fetch_all(&mut *connection)
    .await?;
    if reached.is_empty() {
        return Ok((report_id, Vec::new()));
    }
    sqlx::query("UPDATE strokes SET deleted_at = now() WHERE id = ANY($1) AND deleted_at IS NULL")
        .bind(&reached)
        .execute(&mut *connection)
        .await?;
    sqlx::query("INSERT INTO stroke_holds (stroke_id) SELECT unnest($1::uuid[]) ON CONFLICT (stroke_id) DO UPDATE SET held_at = now(), released_at = NULL, outcome = NULL")
        .bind(&reached)
        .execute(&mut *connection)
        .await?;
    let held = strokes
        .into_iter()
        .filter(|stroke| reached.contains(&stroke.input.id))
        .collect();
    Ok((report_id, held))
}

pub(crate) async fn partition_page(
    connection: &mut PgConnection,
    bounds: Bounds,
    after: i64,
    limit: u32,
    pass: crate::snapshot::Pass,
) -> Result<Vec<StoredStroke>, sqlx::Error> {
    let mut query = format!(
        "SELECT {COLUMNS} FROM strokes WHERE deleted_at IS NULL AND bounds && box(point($1,$2),point($3,$4)) AND seq > $5"
    );
    if pass.filter().is_some() {
        query.push_str(" AND (bounds && box(point($7,$8),point($9,$10))) = $11");
    }
    query.push_str(" ORDER BY seq ASC LIMIT $6");
    let request = sqlx::query_as::<_, StrokeRow>(&query)
        .bind(bounds.min_x)
        .bind(bounds.min_y)
        .bind(bounds.max_x)
        .bind(bounds.max_y)
        .bind(after)
        .bind(i64::from(limit));
    let request = if let Some((visible, inside)) = pass.filter() {
        request
            .bind(visible.min_x)
            .bind(visible.min_y)
            .bind(visible.max_x)
            .bind(visible.max_y)
            .bind(inside)
    } else {
        request
    };
    request
        .fetch_all(connection)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
}

#[derive(FromRow)]
pub(crate) struct ManifestEntry {
    pub id: Uuid,
    pub seq: i64,
    pub point_count: i32,
}

/// Metadata from one consistent snapshot; points are not transferred from PostgreSQL.
pub(crate) async fn manifest_partition_page(
    connection: &mut PgConnection,
    bounds: Bounds,
    after: i64,
    limit: u32,
    pass: crate::snapshot::Pass,
) -> Result<Vec<ManifestEntry>, sqlx::Error> {
    let mut query = String::from(
        "SELECT id, seq, point_count FROM strokes WHERE deleted_at IS NULL AND bounds && box(point($1,$2),point($3,$4)) AND seq > $5",
    );
    if pass.filter().is_some() {
        query.push_str(" AND (bounds && box(point($7,$8),point($9,$10))) = $11");
    }
    query.push_str(" ORDER BY seq ASC LIMIT $6");
    let request = sqlx::query_as(&query)
        .bind(bounds.min_x)
        .bind(bounds.min_y)
        .bind(bounds.max_x)
        .bind(bounds.max_y)
        .bind(after)
        .bind(i64::from(limit));
    let request = if let Some((visible, inside)) = pass.filter() {
        request
            .bind(visible.min_x)
            .bind(visible.min_y)
            .bind(visible.max_x)
            .bind(visible.max_y)
            .bind(inside)
    } else {
        request
    };
    request.fetch_all(connection).await
}

/// Snapshot-member geometry remains immutable after soft deletion. Membership and
/// seq are checked by the manifest stream before these rows reach a socket.
pub(crate) async fn manifest_geometry(
    pool: &sqlx::PgPool,
    ids: &[Uuid],
) -> Result<Vec<StoredStroke>, sqlx::Error> {
    let query = format!("SELECT {COLUMNS} FROM strokes WHERE id = ANY($1)");
    sqlx::query_as::<_, StrokeRow>(&query)
        .bind(ids)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
}
