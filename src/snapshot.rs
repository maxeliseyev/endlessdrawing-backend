//! Bounded snapshot streaming. Live events remain in the broadcast ring until end.

use std::sync::Arc;

use sqlx::{Postgres, Transaction};
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;

use crate::{
    config::RealtimeSettings,
    domain::Bounds,
    protocol::{ErrorCode, PublicStroke, ServerMessage},
    session::OwnerDigest,
    storage,
};

pub(crate) struct SnapshotStream {
    receiver: mpsc::Receiver<ServerMessage>,
    _task: AbortOnDropHandle<()>,
}

impl SnapshotStream {
    pub fn start(
        transaction: Transaction<'static, Postgres>,
        owner: OwnerDigest,
        bounds: Bounds,
        id: String,
        settings: Arc<RealtimeSettings>,
    ) -> Self {
        // At most two serialized-message equivalents wait ahead of the socket writer.
        let (sender, receiver) = mpsc::channel(2);
        let task = tokio::spawn(async move {
            let result = tokio::time::timeout(
                settings.snapshot_timeout,
                stream(transaction, &sender, &owner, bounds, &id, &settings),
            )
            .await;
            if !matches!(result, Ok(Ok(()))) {
                let _ = sender
                    .send(ErrorCode::ResyncRequired.response(None, Some(id)))
                    .await;
            }
        });
        Self {
            receiver,
            _task: AbortOnDropHandle::new(task),
        }
    }

    pub async fn recv(&mut self) -> Option<ServerMessage> {
        self.receiver.recv().await
    }
}

async fn deliver(sender: &mpsc::Sender<ServerMessage>, message: ServerMessage) -> Result<(), ()> {
    sender.send(message).await.map_err(|_| ())
}

async fn stream(
    mut transaction: Transaction<'static, Postgres>,
    sender: &mpsc::Sender<ServerMessage>,
    owner: &OwnerDigest,
    bounds: Bounds,
    id: &str,
    settings: &RealtimeSettings,
) -> Result<(), ()> {
    deliver(
        sender,
        ServerMessage::SnapshotBegin {
            subscription_id: id.into(),
        },
    )
    .await?;
    let overhead = serde_json::to_vec(&ServerMessage::SnapshotChunk {
        subscription_id: id.into(),
        strokes: Vec::new(),
    })
    .map_err(|_| ())?
    .len();
    let mut after = 0;
    loop {
        let page = storage::page(&mut transaction, bounds, after, settings.snapshot_page_size)
            .await
            .map_err(|_| ())?;
        if page.is_empty() {
            break;
        }
        let mut strokes: Vec<PublicStroke> = Vec::new();
        let mut bytes = overhead;
        for record in page {
            after = record.seq;
            let stroke = record.public(owner);
            let length = serde_json::to_vec(&stroke).map_err(|_| ())?.len();
            if overhead + length > settings.snapshot_chunk_bytes {
                return Err(());
            }
            let separator = usize::from(!strokes.is_empty());
            if bytes + length + separator > settings.snapshot_chunk_bytes {
                deliver(
                    sender,
                    ServerMessage::SnapshotChunk {
                        subscription_id: id.into(),
                        strokes: std::mem::take(&mut strokes),
                    },
                )
                .await?;
                bytes = overhead;
            }
            bytes += length + usize::from(!strokes.is_empty());
            strokes.push(stroke);
        }
        if !strokes.is_empty() {
            deliver(
                sender,
                ServerMessage::SnapshotChunk {
                    subscription_id: id.into(),
                    strokes,
                },
            )
            .await?;
        }
    }
    transaction.commit().await.map_err(|_| ())?;
    deliver(
        sender,
        ServerMessage::SnapshotEnd {
            subscription_id: id.into(),
        },
    )
    .await
}
