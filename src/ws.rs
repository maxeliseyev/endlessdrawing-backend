//! One bounded, ordered socket writer per tab. No raw messages are logged.

use std::{collections::VecDeque, future::pending, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{
        HeaderMap, StatusCode,
        header::{ORIGIN, SEC_WEBSOCKET_PROTOCOL},
    },
    response::{IntoResponse, Response},
    routing::get,
};
use tokio::{
    sync::{OwnedSemaphorePermit, broadcast},
    time::Instant,
};
use tokio_tungstenite::tungstenite::Error as SocketError;

use uuid::Uuid;

use crate::{
    codec::{
        CLIENT_BYTES, COMPACT_PROTOCOL, JSON_PROTOCOL, MANIFEST_PROTOCOL, MSGPACK_PROTOCOL,
        PRIORITY_PROTOCOL, Profile, SAFETY_PROTOCOL,
    },
    domain::Bounds,
    protocol::{
        AckResult, ClientMessage, ErrorCode, MAX_REPORT_STROKES, ServerMessage, SpawnPoint,
        intersects, valid_id,
    },
    realtime::{Event, Realtime},
    session::OwnerDigest,
    snapshot::SnapshotStream,
};

pub(crate) fn router(service: Realtime) -> Router {
    Router::new().route("/ws", get(upgrade)).with_state(service)
}

async fn upgrade(
    State(service): State<Realtime>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let mut origins = headers.get_all(ORIGIN).iter();
    let origin = origins.next().and_then(|value| value.to_str().ok());
    if origins.next().is_some()
        || !origin.is_some_and(|origin| {
            service
                .settings
                .allowed_origins
                .iter()
                .any(|allowed| allowed == origin)
        })
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if service.shutdown.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(permit) = service.connections.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let offered: Vec<&str> = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(str::trim))
        .collect();
    let v4 = service.settings.msgpack_enabled
        && service.settings.manifest_enabled
        && service.settings.priority_enabled;
    let v5 = v4 && service.settings.safety_enabled;
    let selected = if v5 && service.settings.compact_enabled && offered.contains(&COMPACT_PROTOCOL)
    {
        Some(COMPACT_PROTOCOL)
    } else if v5 && offered.contains(&SAFETY_PROTOCOL) {
        Some(SAFETY_PROTOCOL)
    } else if v4 && offered.contains(&PRIORITY_PROTOCOL) {
        Some(PRIORITY_PROTOCOL)
    } else if service.settings.msgpack_enabled
        && service.settings.manifest_enabled
        && offered.contains(&MANIFEST_PROTOCOL)
    {
        Some(MANIFEST_PROTOCOL)
    } else if service.settings.msgpack_enabled && offered.contains(&MSGPACK_PROTOCOL) {
        Some(MSGPACK_PROTOCOL)
    } else if offered.contains(&JSON_PROTOCOL) {
        Some(JSON_PROTOCOL)
    } else if headers.contains_key(SEC_WEBSOCKET_PROTOCOL) {
        return StatusCode::BAD_REQUEST.into_response();
    } else {
        None
    };
    let profile = if selected == Some(COMPACT_PROTOCOL) {
        Profile::Compact
    } else if selected == Some(SAFETY_PROTOCOL) {
        Profile::Safety
    } else if selected == Some(PRIORITY_PROTOCOL) {
        Profile::Priority
    } else if selected == Some(MANIFEST_PROTOCOL) {
        Profile::Manifest
    } else if selected == Some(MSGPACK_PROTOCOL) {
        Profile::MessagePack
    } else {
        Profile::Json
    };
    let message_limit = if profile.binary() {
        service.settings.max_message_bytes.min(CLIENT_BYTES)
    } else {
        service.settings.max_message_bytes
    };
    upgrade
        .protocols(selected)
        .max_message_size(message_limit)
        .max_frame_size(message_limit)
        .write_buffer_size(0)
        .on_upgrade(move |socket| connection(socket, service, permit, profile))
}

async fn send(
    profile: Profile,
    socket: &mut WebSocket,
    message: &ServerMessage,
    timeout: Duration,
) -> bool {
    let Ok(frame) = profile.frame(message) else {
        return false;
    };
    matches!(
        tokio::time::timeout(timeout, socket.send(frame)).await,
        Ok(Ok(()))
    )
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str, timeout: Duration) {
    let _ = tokio::time::timeout(
        timeout,
        socket.send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }))),
    )
    .await;
}

// A frame/message capacity error happens before application decode. Preserve its
// close code rather than turning an oversized message into a normal shutdown.
async fn failed_frame(socket: &mut WebSocket, error: axum::Error, timeout: Duration) {
    let code = match error.into_inner().downcast::<SocketError>().as_deref() {
        Ok(SocketError::Capacity(_)) => 1009,
        Ok(SocketError::Utf8(_)) => 1007,
        Ok(SocketError::Protocol(_)) => 1002,
        _ => 1001,
    };
    close(socket, code, "invalid websocket frame", timeout).await;
}

async fn hello(
    socket: &mut WebSocket,
    service: &Realtime,
    profile: Profile,
) -> Option<OwnerDigest> {
    let deadline = Instant::now() + service.settings.hello_timeout;
    loop {
        let frame = tokio::select! {
            _ = service.shutdown.cancelled() => return None,
            result = tokio::time::timeout_at(deadline, socket.recv()) => match result {
                Ok(Some(Ok(frame))) => frame,
                Ok(Some(Err(error))) => { failed_frame(socket, error, service.settings.send_timeout).await; return None; }
                _ => return None,
            },
        };
        match frame {
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return None,
            frame @ (Message::Text(_) | Message::Binary(_)) => {
                let value = match profile.decode(frame) {
                    Ok(value) => value,
                    Err(error) => {
                        close(
                            socket,
                            error.close_code(),
                            "invalid wire message",
                            service.settings.send_timeout,
                        )
                        .await;
                        return None;
                    }
                };
                let result = match serde_json::from_value::<ClientMessage>(value) {
                    Ok(ClientMessage::Hello { session_secret }) => {
                        OwnerDigest::from_secret(&session_secret)
                            .map_err(|_| ErrorCode::InvalidSession)
                    }
                    Ok(_) => Err(ErrorCode::HelloRequired),
                    Err(_) => Err(ErrorCode::InvalidMessage),
                };
                match result {
                    Ok(owner) => {
                        if send(
                            profile,
                            socket,
                            &ServerMessage::HelloAck {
                                protocol_version: profile.version(),
                                spawn: service.spawn.pick().map(|[x, y]| SpawnPoint { x, y }),
                            },
                            service.settings.send_timeout,
                        )
                        .await
                        {
                            return Some(owner);
                        }
                        return None;
                    }
                    Err(code) => {
                        let _ = send(
                            profile,
                            socket,
                            &code.response(None, None),
                            service.settings.send_timeout,
                        )
                        .await;
                        return None;
                    }
                }
            }
        }
    }
}

struct Subscription {
    id: String,
    bounds: Bounds,
    events: broadcast::Receiver<Arc<Event>>,
    snapshot: Option<SnapshotStream>,
}

enum SubscriptionItem {
    Snapshot(ServerMessage),
    Event(Arc<Event>),
    Resync,
}

async fn subscription_item(subscription: &mut Option<Subscription>) -> SubscriptionItem {
    let Some(subscription) = subscription else {
        return pending().await;
    };
    if let Some(snapshot) = &mut subscription.snapshot {
        return snapshot
            .recv()
            .await
            .map(SubscriptionItem::Snapshot)
            .unwrap_or(SubscriptionItem::Resync);
    }
    subscription
        .events
        .recv()
        .await
        .map(|event| {
            if matches!(event.as_ref(), Event::Reset) {
                SubscriptionItem::Resync
            } else {
                SubscriptionItem::Event(event)
            }
        })
        .unwrap_or(SubscriptionItem::Resync)
}

async fn connection(
    mut socket: WebSocket,
    service: Realtime,
    _permit: OwnedSemaphorePermit,
    profile: Profile,
) {
    let Some(owner) = hello(&mut socket, &service, profile).await else {
        close(
            &mut socket,
            1008,
            "hello failed or timed out",
            service.settings.send_timeout,
        )
        .await;
        return;
    };
    let mut subscription = None;
    let mut rates = Rates::new(&service);
    let mut last_seen = Instant::now();
    let mut heartbeat = tokio::time::interval_at(
        last_seen + service.settings.heartbeat_interval,
        service.settings.heartbeat_interval,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let response = tokio::select! {
            _ = service.shutdown.cancelled() => break,
            _ = heartbeat.tick() => {
                if last_seen.elapsed() >= service.settings.idle_timeout { break; }
                if !matches!(tokio::time::timeout(service.settings.send_timeout, socket.send(Message::Ping(Vec::new().into()))).await, Ok(Ok(()))) { break; }
                None
            }
            frame = socket.recv() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    Some(Err(error)) => { failed_frame(&mut socket, error, service.settings.send_timeout).await; break; }
                    None => break,
                };
                last_seen = Instant::now();
                if !rates.frames.allow(last_seen) {
                    let _ = send(profile, &mut socket, &ErrorCode::RateLimited.response(None, None), service.settings.send_timeout).await;
                    break;
                }
                match frame {
                    frame @ (Message::Text(_) | Message::Binary(_)) => {
                        // Legacy malformed JSON retains its correlated invalid_message response.
                        let parsed = match (&frame, profile) {
                            (Message::Text(text), Profile::Json) => parse(text),
                            _ => {
                                let value = match profile.decode(frame) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        close(&mut socket, error.close_code(), "invalid wire message", service.settings.send_timeout).await;
                                        break;
                                    }
                                };
                                parse_value(value)
                            }
                        };
                        match parsed {
                            Ok(message) => command(message, &service, &owner, &mut rates, &mut subscription, profile).await,
                            Err(error) => Some(*error),
                        }
                    },
                    Message::Ping(bytes) => {
                        if !matches!(tokio::time::timeout(service.settings.send_timeout, socket.send(Message::Pong(bytes))).await, Ok(Ok(()))) { break; }
                        None
                    }
                    Message::Pong(_) => None,
                    Message::Close(_) => break,
                }
            }
            item = subscription_item(&mut subscription) => {
                match item {
                    SubscriptionItem::Snapshot(message) => {
                        if matches!(message, ServerMessage::SnapshotEnd { .. }) {
                            if let Some(current) = &mut subscription { current.snapshot = None; }
                        } else if matches!(message, ServerMessage::Error { .. } | ServerMessage::SnapshotFallback { .. }) {
                            subscription = None;
                        }
                        Some(message)
                    }
                    SubscriptionItem::Event(event) => {
                        subscription.as_ref().and_then(|current| {
                            if !intersects(current.bounds, event.bounds()?) { return None; }
                            Some(match event.as_ref() {
                                Event::Created(stroke) => ServerMessage::Created { subscription_id: current.id.clone(), stroke: stroke.public(&owner, profile) },
                                Event::Deleted { id, seq, .. } => ServerMessage::Deleted { subscription_id: current.id.clone(), stroke_id: *id, seq: *seq },
                                Event::Reset => return None,
                            })
                        })
                    }
                    SubscriptionItem::Resync => {
                        subscription.take().map(|current| ErrorCode::ResyncRequired.response(None, Some(current.id.clone())))
                    }
                }
            }
        };
        if let Some(response) = response
            && !send(
                profile,
                &mut socket,
                &response,
                service.settings.send_timeout,
            )
            .await
        {
            break;
        }
    }
    drop(subscription); // Aborts snapshot tasks, rolling back their read transactions.
    close(
        &mut socket,
        1001,
        "connection closing",
        service.settings.send_timeout,
    )
    .await;
}

fn parse(text: &str) -> Result<ClientMessage, Box<ServerMessage>> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| ErrorCode::InvalidMessage.response(None, None))?;
    parse_value(value)
}

fn parse_value(value: serde_json::Value) -> Result<ClientMessage, Box<ServerMessage>> {
    let request_id = value
        .get("request_id")
        .and_then(|value| value.as_str())
        .filter(|value| valid_id(value))
        .map(str::to_owned);
    let subscription_id = value
        .get("subscription_id")
        .and_then(|value| value.as_str())
        .filter(|value| valid_id(value))
        .map(str::to_owned);
    serde_json::from_value(value)
        .map_err(|_| Box::new(ErrorCode::InvalidMessage.response(request_id, subscription_id)))
}

async fn command(
    message: ClientMessage,
    service: &Realtime,
    owner: &OwnerDigest,
    rates: &mut Rates,
    subscription: &mut Option<Subscription>,
    profile: Profile,
) -> Option<ServerMessage> {
    let cached = matches!(&message, ClientMessage::CachedSubscribe { .. });
    match message {
        ClientMessage::Hello { .. } => Some(ErrorCode::InvalidMessage.response(None, None)),
        ClientMessage::Subscribe {
            subscription_id,
            bounds,
            visible_bounds,
        }
        | ClientMessage::CachedSubscribe {
            subscription_id,
            bounds,
            visible_bounds,
        } => {
            if cached && !profile.manifest() {
                return Some(ErrorCode::InvalidMessage.response(None, Some(subscription_id)));
            }
            if !valid_id(&subscription_id) {
                return Some(ErrorCode::InvalidMessage.response(None, None));
            }
            if !rates.subscribes.allow(Instant::now()) {
                return Some(ErrorCode::RateLimited.response(None, Some(subscription_id)));
            }
            let bounds = match bounds.validate(service.settings.viewport_max_span) {
                Ok(bounds) => bounds,
                Err(code) => return Some(code.response(None, Some(subscription_id))),
            };
            if visible_bounds.is_some() != profile.visible_first() {
                return Some(ErrorCode::InvalidMessage.response(None, Some(subscription_id)));
            }
            let visible = match visible_bounds {
                Some(input) => match input.validate(service.settings.viewport_max_span) {
                    Ok(visible)
                        if visible.min_x >= bounds.min_x
                            && visible.min_y >= bounds.min_y
                            && visible.max_x <= bounds.max_x
                            && visible.max_y <= bounds.max_y =>
                    {
                        Some(visible)
                    }
                    _ => {
                        return Some(
                            ErrorCode::ValidationFailed.response(None, Some(subscription_id)),
                        );
                    }
                },
                None => None,
            };
            let area = crate::snapshot::Area { bounds, visible };
            // Release an old read transaction before acquiring a connection for the next.
            *subscription = None;
            let permit = if cached {
                match service.manifest_slots.clone().try_acquire_owned() {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        return Some(ServerMessage::SnapshotFallback {
                            subscription_id,
                            reason: "busy",
                        });
                    }
                }
            } else {
                None
            };
            match service.writer.snapshot().await {
                Ok(snapshot) => {
                    let stream = if let Some(permit) = permit {
                        crate::manifest::start(
                            snapshot.transaction,
                            owner.clone(),
                            area,
                            subscription_id.clone(),
                            service.clone(),
                            permit,
                            profile,
                        )
                    } else {
                        SnapshotStream::start(
                            snapshot.transaction,
                            owner.clone(),
                            area,
                            subscription_id.clone(),
                            service.settings.clone(),
                            profile,
                        )
                    };
                    *subscription = Some(Subscription {
                        id: subscription_id,
                        bounds,
                        events: snapshot.events,
                        snapshot: Some(stream),
                    });
                    None
                }
                Err(code) => Some(code.response(None, Some(subscription_id))),
            }
        }
        ClientMessage::Fetch {
            subscription_id,
            snapshot_token,
            batch,
            stroke_ids,
            final_batch,
        } => {
            if !profile.manifest() {
                return Some(ErrorCode::InvalidMessage.response(None, Some(subscription_id)));
            }
            // Stale requests cannot terminate a replacement subscription.
            let current = subscription
                .as_ref()
                .filter(|current| current.id == subscription_id)?;
            let valid = stroke_ids.len() <= 1000
                && batch <= 20
                && current.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.fetch(crate::manifest::FetchRequest {
                        subscription_id: subscription_id.clone(),
                        snapshot_token,
                        batch,
                        stroke_ids,
                        final_batch,
                    })
                });
            if !valid {
                *subscription = None;
                return Some(ErrorCode::ResyncRequired.response(None, Some(subscription_id)));
            }
            None
        }
        ClientMessage::Create { request_id, stroke } => {
            if !valid_id(&request_id) {
                return Some(ErrorCode::InvalidMessage.response(None, None));
            }
            let result = if !rates.creates.allow(Instant::now()) {
                Err(ErrorCode::RateLimited)
            } else {
                match stroke.validate(service.limits) {
                    Ok(stroke) => service.writer.create(stroke, owner.clone()).await,
                    Err(_) => Err(ErrorCode::ValidationFailed),
                }
            };
            Some(match result {
                Ok(mut result) => {
                    if let AckResult::Create { stroke } = &mut result {
                        if profile.safety() {
                            stroke.author = Some(owner.author_tag(owner.as_bytes()));
                        }
                        stroke.compact = profile.compact();
                    }
                    ServerMessage::Ack { request_id, result }
                }
                Err(code) => code.response(Some(request_id), None),
            })
        }
        ClientMessage::Report {
            request_id,
            stroke_ids,
            reason,
            block_author,
        } => {
            if !profile.safety() || !valid_id(&request_id) {
                return Some(ErrorCode::InvalidMessage.response(None, None));
            }
            if let Some(report_id) = rates.reported(&request_id) {
                let result = AckResult::Report { report_id };
                return Some(ServerMessage::Ack { request_id, result });
            }
            let unique: std::collections::HashSet<_> = stroke_ids.iter().collect();
            if stroke_ids.is_empty()
                || stroke_ids.len() > MAX_REPORT_STROKES
                || unique.len() != stroke_ids.len()
            {
                return Some(ErrorCode::ValidationFailed.response(Some(request_id), None));
            }
            let result = if rates.deletes.allow(Instant::now()) {
                service
                    .writer
                    .report(owner.clone(), stroke_ids, reason, block_author)
                    .await
            } else {
                Err(ErrorCode::RateLimited)
            };
            Some(match result {
                Ok(result) => {
                    if let AckResult::Report { report_id } = &result {
                        rates.remember_report(request_id.clone(), *report_id);
                    }
                    ServerMessage::Ack { request_id, result }
                }
                Err(code) => code.response(Some(request_id), None),
            })
        }
        ClientMessage::Delete {
            request_id,
            stroke_id,
        } => {
            if !valid_id(&request_id) {
                return Some(ErrorCode::InvalidMessage.response(None, None));
            }
            let result = if rates.deletes.allow(Instant::now()) {
                service.writer.delete(stroke_id, owner.clone()).await
            } else {
                Err(ErrorCode::RateLimited)
            };
            Some(match result {
                Ok(result) => ServerMessage::Ack { request_id, result },
                Err(code) => code.response(Some(request_id), None),
            })
        }
    }
}

struct Rates {
    frames: Window,
    creates: Window,
    deletes: Window,
    subscribes: Window,
    /// Recent successful `stroke.report` request ids: a retried request returns the same ack.
    reports: VecDeque<(String, Uuid)>,
}

impl Rates {
    fn new(service: &Realtime) -> Self {
        Self {
            frames: Window::new(service.settings.messages_per_second),
            creates: Window::new(service.settings.creates_per_second),
            deletes: Window::new(service.settings.deletes_per_second),
            subscribes: Window::new(service.settings.subscribes_per_second),
            reports: VecDeque::new(),
        }
    }

    fn reported(&self, request_id: &str) -> Option<Uuid> {
        self.reports
            .iter()
            .find(|(id, _)| id == request_id)
            .map(|(_, report)| *report)
    }

    fn remember_report(&mut self, request_id: String, report_id: Uuid) {
        if self.reports.len() >= 32 {
            self.reports.pop_front();
        }
        self.reports.push_back((request_id, report_id));
    }
}

struct Window {
    limit: usize,
    timestamps: VecDeque<Instant>,
}

impl Window {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            timestamps: VecDeque::new(),
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        while self
            .timestamps
            .front()
            .is_some_and(|time| now.duration_since(*time) >= Duration::from_secs(1))
        {
            self.timestamps.pop_front();
        }
        if self.timestamps.len() >= self.limit {
            return false;
        }
        self.timestamps.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliding_window_does_not_allow_boundary_bursts() {
        let mut window = Window::new(2);
        let start = Instant::now();
        assert!(window.allow(start));
        assert!(window.allow(start + Duration::from_millis(900)));
        assert!(!window.allow(start + Duration::from_millis(999)));
        assert!(window.allow(start + Duration::from_secs(1)));
        assert!(!window.allow(start + Duration::from_millis(1001)));
        assert!(window.allow(start + Duration::from_millis(1900)));
    }

    #[tokio::test]
    async fn lagged_subscription_requires_resync_instead_of_skipping_events() {
        let (sender, receiver) = broadcast::channel(1);
        let bounds = Bounds {
            min_x: -10.0,
            min_y: -10.0,
            max_x: 10.0,
            max_y: 10.0,
        };
        let mut subscription = Some(Subscription {
            id: "v1".into(),
            bounds,
            events: receiver,
            snapshot: None,
        });
        for seq in 1..=3 {
            let _ = sender.send(Arc::new(Event::Deleted {
                id: uuid::Uuid::new_v4(),
                seq,
                bounds,
            }));
        }
        assert!(matches!(
            subscription_item(&mut subscription).await,
            SubscriptionItem::Resync
        ));
    }

    #[tokio::test]
    async fn uncertain_commit_invalidates_live_subscriptions() {
        let (sender, receiver) = broadcast::channel(4);
        let bounds = Bounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 10.0,
            max_y: 10.0,
        };
        let mut subscription = Some(Subscription {
            id: "v1".into(),
            bounds,
            events: receiver,
            snapshot: None,
        });
        sender.send(Arc::new(Event::Reset)).ok().unwrap();
        assert!(matches!(
            subscription_item(&mut subscription).await,
            SubscriptionItem::Resync
        ));
    }

    #[test]
    fn malformed_mutations_preserve_only_valid_correlation_ids() {
        let error = parse(r#"{"type":"stroke.create","request_id":"r1","stroke":{"seq":1}}"#)
            .err()
            .unwrap();
        let ServerMessage::Error {
            request_id, code, ..
        } = *error
        else {
            panic!("expected error");
        };
        assert_eq!(request_id.as_deref(), Some("r1"));
        assert_eq!(code, ErrorCode::InvalidMessage);
    }
}
