//! Bounded snapshot streaming. Live events remain in the broadcast ring until end.

use std::sync::Arc;

use sqlx::{Postgres, Transaction};
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;

use crate::{
    codec::{Profile, SERVER_BYTES},
    config::RealtimeSettings,
    domain::Bounds,
    protocol::{ErrorCode, PublicStroke, ServerMessage},
    session::OwnerDigest,
    storage,
};

/// The hint classifies members; it never expands the authoritative area.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Area {
    pub bounds: Bounds,
    pub visible: Option<Bounds>,
}

#[derive(Clone, Copy)]
pub(crate) enum Pass {
    All,
    Visible(Bounds),
    Prefetch(Bounds),
}

impl Area {
    pub fn passes(self) -> impl Iterator<Item = Pass> {
        match self.visible {
            Some(bounds) => [Some(Pass::Visible(bounds)), Some(Pass::Prefetch(bounds))],
            None => [Some(Pass::All), None],
        }
        .into_iter()
        .flatten()
    }
}

impl Pass {
    pub fn filter(self) -> Option<(Bounds, bool)> {
        match self {
            Self::All => None,
            Self::Visible(bounds) => Some((bounds, true)),
            Self::Prefetch(bounds) => Some((bounds, false)),
        }
    }
}

pub(crate) struct SnapshotStream {
    pub(crate) receiver: mpsc::Receiver<ServerMessage>,
    pub(crate) requests: Option<mpsc::Sender<crate::manifest::FetchRequest>>,
    pub(crate) _task: AbortOnDropHandle<()>,
}

impl SnapshotStream {
    pub fn fetch(&self, request: crate::manifest::FetchRequest) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|requests| requests.try_send(request).is_ok())
    }

    pub fn start(
        transaction: Transaction<'static, Postgres>,
        owner: OwnerDigest,
        area: Area,
        id: String,
        settings: Arc<RealtimeSettings>,
        profile: Profile,
    ) -> Self {
        // At most two serialized-message equivalents wait ahead of the socket writer.
        let (sender, receiver) = mpsc::channel(2);
        let task = tokio::spawn(async move {
            let result = tokio::time::timeout(
                settings.snapshot_timeout,
                stream(transaction, &sender, &owner, area, &id, &settings, profile),
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
            requests: None,
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
    area: Area,
    id: &str,
    settings: &RealtimeSettings,
    profile: Profile,
) -> Result<(), ()> {
    deliver(
        sender,
        ServerMessage::SnapshotBegin {
            subscription_id: id.into(),
        },
    )
    .await?;
    for pass in area.passes() {
        let mut after = 0;
        loop {
            let page = storage::partition_page(
                &mut transaction,
                area.bounds,
                after,
                settings.snapshot_page_size,
                pass,
            )
            .await
            .map_err(|_| ())?;
            if page.is_empty() {
                break;
            }
            if let Some(last) = page.last() {
                after = last.seq;
            }
            deliver_strokes(
                sender,
                id,
                settings,
                profile,
                page.into_iter().map(|record| record.public(owner)),
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

pub(crate) async fn deliver_strokes(
    sender: &mpsc::Sender<ServerMessage>,
    id: &str,
    settings: &RealtimeSettings,
    profile: Profile,
    records: impl Iterator<Item = PublicStroke> + Send,
) -> Result<(), ()> {
    let overhead = profile
        .encode(&ServerMessage::SnapshotChunk {
            subscription_id: id.into(),
            strokes: Vec::new(),
        })
        .map_err(|_| ())?
        .len();
    // Array headers can grow by up to four bytes after the empty-array estimate.
    let overhead = overhead + if profile.binary() { 4 } else { 0 };
    let hard_limit = if profile.binary() {
        settings.snapshot_chunk_bytes.min(SERVER_BYTES)
    } else {
        settings.snapshot_chunk_bytes
    };
    let target = if profile.binary() {
        hard_limit.min(64 * 1024)
    } else {
        hard_limit
    };
    let mut strokes: Vec<PublicStroke> = Vec::new();
    let mut bytes = overhead;
    let mut points = 0;
    for stroke in records {
        let length = profile.encode(&stroke).map_err(|_| ())?.len();
        if overhead + length > hard_limit {
            return Err(());
        }
        let separator = usize::from(!strokes.is_empty());
        if !strokes.is_empty()
            && (bytes + length + separator > target
                || strokes.len() >= 1000
                || (profile.binary() && points + stroke.input.points.len() > 8192))
        {
            deliver(
                sender,
                ServerMessage::SnapshotChunk {
                    subscription_id: id.into(),
                    strokes: std::mem::take(&mut strokes),
                },
            )
            .await?;
            bytes = overhead;
            points = 0;
        }
        bytes += length + usize::from(!strokes.is_empty());
        points += stroke.input.points.len();
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
    Ok(())
}
