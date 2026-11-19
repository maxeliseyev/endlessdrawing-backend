//! Single writer, ordered post-commit events and snapshot barriers.

use std::{sync::Arc, time::Duration};

use axum::Router;
use sqlx::{PgPool, Postgres, Transaction};
use tokio::{
    sync::{Semaphore, broadcast, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    admin::{AdminError, HideResult, SessionProof, ValidatedHide, execute_hide},
    config::{ConfigError, RealtimeSettings},
    domain::{Bounds, StrokeLimits, ValidatedStroke},
    protocol::{AckResult, ErrorCode, ReportReason},
    session::OwnerDigest,
    spawn::SpawnPoints,
    storage::{self, StorageError, StoredStroke},
};

/// Shared realtime service. Start exactly one per backend process/database.
#[derive(Clone)]
pub struct Realtime {
    pub(crate) writer: Writer,
    pub(crate) settings: Arc<RealtimeSettings>,
    pub(crate) limits: StrokeLimits,
    pub(crate) connections: Arc<Semaphore>,
    pub(crate) manifest_slots: Arc<Semaphore>,
    pub(crate) pool: PgPool,
    pub(crate) shutdown: CancellationToken,
    pub(crate) spawn: SpawnPoints,
}

impl Realtime {
    /// Starts the bounded sequential mutation worker inside a Tokio runtime.
    /// Retain/await its handle on shutdown; cancel through `shutdown_token()` first.
    ///
    /// # Errors
    /// Returns a safe configuration error for invalid resource settings.
    pub fn start(
        pool: PgPool,
        limits: StrokeLimits,
        settings: RealtimeSettings,
    ) -> Result<(Self, JoinHandle<()>), ConfigError> {
        settings.validate()?;
        let (sender, receiver) = mpsc::channel(settings.mutation_queue_capacity);
        let (events, _) = broadcast::channel(settings.event_buffer_capacity);
        let shutdown = CancellationToken::new();
        let spawn = SpawnPoints::default();
        spawn.run(pool.clone(), settings.spawn_refresh, shutdown.clone());
        let policy = storage::ReportPolicy {
            per_hour: i64::try_from(settings.reports_per_hour).unwrap_or(i64::MAX),
            hold_threshold: i64::try_from(settings.report_hold_threshold).unwrap_or(i64::MAX),
        };
        let worker = tokio::spawn(run_writer(
            pool.clone(),
            receiver,
            events,
            shutdown.clone(),
            policy,
        ));
        let service = Self {
            writer: Writer {
                sender,
                timeout: settings.operation_timeout,
                shutdown: shutdown.clone(),
            },
            limits,
            pool,
            manifest_slots: Arc::new(Semaphore::new(crate::manifest::MAX_CONCURRENT)),
            connections: Arc::new(Semaphore::new(settings.max_connections)),
            settings: Arc::new(settings),
            shutdown,
            spawn,
        };
        Ok((service, worker))
    }

    /// Routes to merge with the health/static router. Only `/ws` is added.
    pub fn router(&self) -> Router {
        crate::ws::router(self.clone())
    }

    /// Cancels all sockets and stops accepting queued mutations.
    /// An already executing mutation is allowed to finish and commit/publish.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }
}

#[derive(Clone)]
pub(crate) struct Writer {
    sender: mpsc::Sender<Command>,
    timeout: Duration,
    shutdown: CancellationToken,
}

pub(crate) enum Event {
    Created(Arc<StoredStroke>),
    Deleted { id: Uuid, seq: i64, bounds: Bounds },
    Reset,
}

impl Event {
    pub fn bounds(&self) -> Option<Bounds> {
        match self {
            Self::Created(stroke) => Some(stroke.bounds),
            Self::Deleted { bounds, .. } => Some(*bounds),
            Self::Reset => None,
        }
    }
}

pub(crate) struct Snapshot {
    pub transaction: Transaction<'static, Postgres>,
    pub events: broadcast::Receiver<Arc<Event>>,
}

enum Command {
    Create {
        stroke: ValidatedStroke,
        owner: OwnerDigest,
        reply: oneshot::Sender<Result<AckResult, ErrorCode>>,
    },
    Delete {
        id: Uuid,
        owner: OwnerDigest,
        reply: oneshot::Sender<Result<AckResult, ErrorCode>>,
    },
    Snapshot {
        reply: oneshot::Sender<Result<Snapshot, ErrorCode>>,
    },
    Report {
        reporter: OwnerDigest,
        ids: Vec<Uuid>,
        reason: ReportReason,
        block_author: bool,
        reply: oneshot::Sender<Result<AckResult, ErrorCode>>,
    },
    AdminHide {
        proof: SessionProof,
        input: ValidatedHide,
        reply: oneshot::Sender<Result<HideResult, AdminError>>,
    },
}

impl Writer {
    pub async fn create(
        &self,
        stroke: ValidatedStroke,
        owner: OwnerDigest,
    ) -> Result<AckResult, ErrorCode> {
        self.request(|reply| Command::Create {
            stroke,
            owner,
            reply,
        })
        .await
    }

    pub async fn delete(&self, id: Uuid, owner: OwnerDigest) -> Result<AckResult, ErrorCode> {
        self.request(|reply| Command::Delete { id, owner, reply })
            .await
    }

    pub async fn report(
        &self,
        reporter: OwnerDigest,
        ids: Vec<Uuid>,
        reason: ReportReason,
        block_author: bool,
    ) -> Result<AckResult, ErrorCode> {
        self.request(|reply| Command::Report {
            reporter,
            ids,
            reason,
            block_author,
            reply,
        })
        .await
    }

    pub async fn snapshot(&self) -> Result<Snapshot, ErrorCode> {
        self.request(|reply| Command::Snapshot { reply }).await
    }

    pub(crate) async fn admin_hide(
        &self,
        proof: SessionProof,
        input: ValidatedHide,
    ) -> Result<HideResult, AdminError> {
        self.request(|reply| Command::AdminHide {
            proof,
            input,
            reply,
        })
        .await
    }

    async fn request<T, E: From<ErrorCode>>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, E>>) -> Command,
    ) -> Result<T, E> {
        let (reply, response) = oneshot::channel();
        self.sender.try_send(build(reply)).map_err(|error| {
            E::from(match error {
                mpsc::error::TrySendError::Full(_) => ErrorCode::RateLimited,
                mpsc::error::TrySendError::Closed(_) => ErrorCode::InternalError,
            })
        })?;
        tokio::select! {
            _ = self.shutdown.cancelled() => Err(E::from(ErrorCode::InternalError)),
            result = tokio::time::timeout(self.timeout, response) => {
                result.map_err(|_| E::from(ErrorCode::InternalError))?.map_err(|_| E::from(ErrorCode::InternalError))?
            }
        }
    }
}

async fn run_writer(
    pool: PgPool,
    mut receiver: mpsc::Receiver<Command>,
    events: broadcast::Sender<Arc<Event>>,
    shutdown: CancellationToken,
    policy: storage::ReportPolicy,
) {
    loop {
        let command = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            command = receiver.recv() => match command { Some(command) => command, None => break },
        };
        match command {
            Command::Create {
                stroke,
                owner,
                reply,
            } => {
                if reply.is_closed() {
                    continue;
                }
                let result = create(&pool, stroke, &owner).await;
                if matches!(result, Err(StorageError::CommitUnknown(_))) {
                    let _ = events.send(Arc::new(Event::Reset));
                }
                let result = result.map(|(record, changed)| {
                    let record = Arc::new(record);
                    if changed {
                        let _ = events.send(Arc::new(Event::Created(record.clone())));
                    }
                    // The connection adds the v5 author tag (its own) before sending.
                    AckResult::Create {
                        stroke: record.public(&owner, false),
                    }
                });
                let _ = reply.send(result.map_err(public_error));
            }
            Command::Delete { id, owner, reply } => {
                if reply.is_closed() {
                    continue;
                }
                let result = delete(&pool, id, &owner).await;
                if matches!(result, Err(StorageError::CommitUnknown(_))) {
                    let _ = events.send(Arc::new(Event::Reset));
                }
                let result = result.map(|(record, changed)| {
                    if changed {
                        let _ = events.send(Arc::new(Event::Deleted {
                            id,
                            seq: record.seq,
                            bounds: record.bounds,
                        }));
                    }
                    AckResult::Delete {
                        stroke_id: id,
                        seq: record.seq,
                        already_deleted: !changed,
                    }
                });
                let _ = reply.send(result.map_err(public_error));
            }
            Command::Report {
                reporter,
                ids,
                reason,
                block_author,
                reply,
            } => {
                if reply.is_closed() {
                    continue;
                }
                let result = report(&pool, &reporter, &ids, reason, block_author, &policy).await;
                if matches!(result, Err(StorageError::CommitUnknown(_))) {
                    let _ = events.send(Arc::new(Event::Reset));
                }
                let result = result.map(|(report_id, held)| {
                    for stroke in held {
                        let _ = events.send(Arc::new(Event::Deleted {
                            id: stroke.input.id,
                            seq: stroke.seq,
                            bounds: stroke.bounds,
                        }));
                    }
                    AckResult::Report { report_id }
                });
                let _ = reply.send(result.map_err(public_error));
            }
            Command::AdminHide {
                proof,
                input,
                reply,
            } => {
                if reply.is_closed() {
                    continue;
                }
                let result = execute_hide(&pool, proof, input)
                    .await
                    .map(|(result, deleted)| {
                        for stroke in deleted {
                            let _ = events.send(Arc::new(Event::Deleted {
                                id: stroke.id,
                                seq: stroke.seq,
                                bounds: stroke.bounds,
                            }));
                        }
                        result
                    });
                if matches!(result, Err(AdminError::CommitUnknown)) {
                    let _ = events.send(Arc::new(Event::Reset));
                }
                let _ = reply.send(result);
            }
            Command::Snapshot { reply } => {
                if reply.is_closed() {
                    continue;
                }
                let result = begin_snapshot(&pool, &events)
                    .await
                    .map_err(|error| public_error(error.into()));
                let _ = reply.send(result);
            }
        }
    }
    // Dropping the receiver rejects remaining commands and their reply channels.
}

async fn report(
    pool: &PgPool,
    reporter: &OwnerDigest,
    ids: &[Uuid],
    reason: ReportReason,
    block_author: bool,
    policy: &storage::ReportPolicy,
) -> Result<(Uuid, Vec<StoredStroke>), StorageError> {
    let mut transaction = pool.begin().await?;
    let result = storage::report(
        &mut transaction,
        reporter,
        ids,
        reason,
        block_author,
        policy,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(StorageError::CommitUnknown)?;
    Ok(result)
}

async fn create(
    pool: &PgPool,
    stroke: ValidatedStroke,
    owner: &OwnerDigest,
) -> Result<(StoredStroke, bool), StorageError> {
    let mut transaction = pool.begin().await?;
    let result = storage::create(&mut transaction, stroke, owner).await?;
    transaction
        .commit()
        .await
        .map_err(StorageError::CommitUnknown)?;
    Ok(result)
}

async fn delete(
    pool: &PgPool,
    id: Uuid,
    owner: &OwnerDigest,
) -> Result<(StoredStroke, bool), StorageError> {
    let mut transaction = pool.begin().await?;
    let result = storage::delete(&mut transaction, id, owner).await?;
    transaction
        .commit()
        .await
        .map_err(StorageError::CommitUnknown)?;
    Ok(result)
}

async fn begin_snapshot(
    pool: &PgPool,
    events: &broadcast::Sender<Arc<Event>>,
) -> Result<Snapshot, sqlx::Error> {
    // The writer cannot mutate between establishing the DB snapshot and subscribing.
    // Merely issuing BEGIN is insufficient: PostgreSQL establishes RR state on the first query.
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("SELECT pg_current_snapshot()")
        .fetch_one(&mut *transaction)
        .await?;
    let receiver = events.subscribe();
    Ok(Snapshot {
        transaction,
        events: receiver,
    })
}

fn public_error(error: StorageError) -> ErrorCode {
    match error {
        StorageError::Rejected(code) => code,
        StorageError::Database(error) | StorageError::CommitUnknown(error) => {
            // Log SQLSTATE only, never row details, owner digests or request bodies.
            let sqlstate = error.as_database_error().and_then(|error| error.code());
            tracing::warn!(sqlstate = ?sqlstate, "database operation failed");
            ErrorCode::InternalError
        }
    }
}

#[cfg(test)]
#[path = "realtime_tests.rs"]
mod tests;
