//! Bounded snapshot membership with on-demand immutable geometry. No transaction
//! or database connection is held while waiting for the client to request points.

use std::collections::{HashMap, HashSet};

use sqlx::{Postgres, Transaction};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

use crate::{
    codec::Profile,
    protocol::{ErrorCode, ServerMessage},
    realtime::Realtime,
    session::OwnerDigest,
    snapshot::{Area, SnapshotStream, deliver_strokes},
    storage,
};

pub(crate) const MAX_CONCURRENT: usize = 16;
const MAX_STROKES: usize = 20_000;
const MAX_POINTS: usize = 1_000_000;
const MAX_FETCH: usize = 1000;
const GEOMETRY_PAGE: usize = 100;

pub(crate) struct FetchRequest {
    pub subscription_id: String,
    pub snapshot_token: Uuid,
    pub batch: u32,
    pub stroke_ids: Vec<Uuid>,
    pub final_batch: bool,
}

struct Context {
    service: Realtime,
    owner: OwnerDigest,
    profile: Profile,
    area: Area,
    id: String,
}

pub(crate) fn start(
    transaction: Transaction<'static, Postgres>,
    owner: OwnerDigest,
    area: Area,
    id: String,
    service: Realtime,
    permit: OwnedSemaphorePermit,
    profile: Profile,
) -> SnapshotStream {
    let (sender, receiver) = mpsc::channel(2);
    let (requests, input) = mpsc::channel(2);
    let context = Context {
        service,
        owner,
        profile,
        area,
        id,
    };
    let task = tokio::spawn(async move {
        let _permit = permit;
        let result = tokio::time::timeout(
            context.service.settings.snapshot_timeout,
            stream(transaction, &sender, input, &context),
        )
        .await;
        if !matches!(result, Ok(Ok(()))) {
            let _ = sender
                .send(ErrorCode::ResyncRequired.response(None, Some(context.id)))
                .await;
        }
    });
    SnapshotStream {
        receiver,
        requests: Some(requests),
        _task: AbortOnDropHandle::new(task),
    }
}

async fn stream(
    mut transaction: Transaction<'static, Postgres>,
    sender: &mpsc::Sender<ServerMessage>,
    mut requests: mpsc::Receiver<FetchRequest>,
    context: &Context,
) -> Result<(), ()> {
    sender
        .send(ServerMessage::SnapshotBegin {
            subscription_id: context.id.clone(),
        })
        .await
        .map_err(|_| ())?;
    let mut entries = Vec::new();
    let mut points = 0_usize;
    for pass in context.area.passes() {
        let mut after = 0;
        loop {
            let page = storage::manifest_partition_page(
                &mut transaction,
                context.area.bounds,
                after,
                context.service.settings.snapshot_page_size,
                pass,
            )
            .await
            .map_err(|_| ())?;
            if page.is_empty() {
                break;
            }
            for entry in page {
                after = entry.seq;
                if entry.point_count < 1
                    || entry.point_count > 2000
                    || entry.seq <= 0
                    || entry.seq > 9_007_199_254_740_991
                {
                    return Err(());
                }
                points += usize::try_from(entry.point_count).map_err(|_| ())?;
                if entries.len() >= MAX_STROKES || points > MAX_POINTS {
                    // Do not claim a complete membership set after truncation.
                    drop(transaction);
                    return sender
                        .send(ServerMessage::SnapshotFallback {
                            subscription_id: context.id.clone(),
                            reason: "scene_limit",
                        })
                        .await
                        .map_err(|_| ());
                }
                entries.push(entry);
            }
        }
    }
    transaction.commit().await.map_err(|_| ())?;
    let token = Uuid::new_v4();
    let id = &context.id;
    sender
        .send(ServerMessage::ManifestBegin {
            subscription_id: id.clone(),
            snapshot_token: token,
            stroke_count: entries.len(),
            point_count: points,
        })
        .await
        .map_err(|_| ())?;
    for page in entries.chunks(MAX_FETCH) {
        sender
            .send(ServerMessage::ManifestChunk {
                subscription_id: id.clone(),
                snapshot_token: token,
                stroke_ids: page.iter().map(|entry| entry.id).collect(),
            })
            .await
            .map_err(|_| ())?;
    }
    sender
        .send(ServerMessage::ManifestEnd {
            subscription_id: id.clone(),
            snapshot_token: token,
        })
        .await
        .map_err(|_| ())?;
    let membership: HashMap<_, _> = entries
        .into_iter()
        .map(|entry| (entry.id, entry.seq))
        .collect();
    let mut requested = HashSet::new();
    let mut batch = 0;
    loop {
        let request = requests.recv().await.ok_or(())?;
        if request.subscription_id != *id
            || request.snapshot_token != token
            || request.batch != batch
            || request.stroke_ids.len() > MAX_FETCH
            || (request.stroke_ids.is_empty() && !request.final_batch)
        {
            return Err(());
        }
        for stroke_id in &request.stroke_ids {
            if !membership.contains_key(stroke_id) || !requested.insert(*stroke_id) {
                return Err(());
            }
        }
        for page in request.stroke_ids.chunks(GEOMETRY_PAGE) {
            let records = storage::manifest_geometry(&context.service.pool, page)
                .await
                .map_err(|_| ())?;
            if records.len() != page.len()
                || records
                    .iter()
                    .any(|record| membership.get(&record.input.id) != Some(&record.seq))
            {
                return Err(());
            }
            let mut records: HashMap<_, _> = records
                .into_iter()
                .map(|record| (record.input.id, record))
                .collect();
            let ordered = page
                .iter()
                .map(|id| records.remove(id).ok_or(()))
                .collect::<Result<Vec<_>, _>>()?;
            deliver_strokes(
                sender,
                id,
                &context.service.settings,
                context.profile,
                ordered
                    .into_iter()
                    .map(|record| record.public(&context.owner, context.profile)),
            )
            .await?;
        }
        if request.final_batch {
            return sender
                .send(ServerMessage::SnapshotEnd {
                    subscription_id: id.clone(),
                })
                .await
                .map_err(|_| ());
        }
        sender
            .send(ServerMessage::FetchEnd {
                subscription_id: id.clone(),
                snapshot_token: token,
                batch,
            })
            .await
            .map_err(|_| ())?;
        batch += 1;
    }
}
