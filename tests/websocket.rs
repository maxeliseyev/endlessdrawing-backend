//! Real HTTP/WebSocket + PostgreSQL tests. Run with --include-ignored and DATABASE_URL.

use std::time::Duration;

use axum::http::HeaderValue;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use endless_drawing_server::{
    app, config::RealtimeSettings, domain::StrokeLimits, realtime::Realtime,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Server {
    url: String,
    service: Realtime,
    http: AbortOnDropHandle<()>,
    writer: AbortOnDropHandle<()>,
}

impl Server {
    async fn start(pool: PgPool, settings: RealtimeSettings) -> Self {
        let (service, writer) =
            Realtime::start(pool.clone(), StrokeLimits::default(), settings).unwrap();
        let router = app::router(pool, Duration::from_secs(1), None).merge(service.router());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let shutdown = service.shutdown_token();
        let http = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            url,
            service,
            http: AbortOnDropHandle::new(http),
            writer: AbortOnDropHandle::new(writer),
        }
    }

    async fn connect(&self, seed: u8) -> Socket {
        let mut request = self.url.clone().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_static("http://localhost:5173"));
        let (mut socket, _) = connect_async(request).await.unwrap();
        send(
            &mut socket,
            json!({"type":"hello", "session_secret":URL_SAFE_NO_PAD.encode([seed; 32])}),
        )
        .await;
        assert_eq!(
            receive(&mut socket).await,
            json!({"type":"hello.ack","protocol_version":1})
        );
        socket
    }

    async fn stop(&mut self) {
        self.service.shutdown_token().cancel();
        timeout(Duration::from_secs(5), &mut self.http)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), &mut self.writer)
            .await
            .unwrap()
            .unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.service.shutdown_token().cancel();
    }
}

async fn send(socket: &mut Socket, message: Value) {
    socket
        .send(Message::Text(message.to_string().into()))
        .await
        .unwrap();
}

async fn receive(socket: &mut Socket) -> Value {
    timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(data) => socket.send(Message::Pong(data)).await.unwrap(),
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for a WS message")
}

async fn until(socket: &mut Socket, kind: &str) -> Value {
    for _ in 0..1000 {
        let message = receive(socket).await;
        if message["type"] == kind {
            return message;
        }
        assert_ne!(message["type"], "error", "unexpected error: {message}");
    }
    panic!("expected message not received: {kind}");
}

fn stroke(id: Uuid) -> Value {
    json!({"id":id,"points":[[0,0],[10,5]],"brush":"round","width":6,"color":"#aabbcc","opacity":0.8})
}

async fn create(socket: &mut Socket, request: &str, payload: Value) -> Value {
    send(
        socket,
        json!({"type":"stroke.create","request_id":request,"stroke":payload}),
    )
    .await;
    until(socket, "ack").await
}

async fn subscribe(socket: &mut Socket, id: &str) -> Vec<Value> {
    send(socket, json!({"type":"viewport.subscribe","subscription_id":id,"bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100}})).await;
    assert_eq!(
        until(socket, "viewport.snapshot.begin").await["subscription_id"],
        id
    );
    let mut strokes = Vec::new();
    loop {
        let message = receive(socket).await;
        assert_eq!(message["subscription_id"], id);
        match message["type"].as_str().unwrap() {
            "viewport.snapshot.chunk" => {
                strokes.extend(message["strokes"].as_array().unwrap().iter().cloned())
            }
            "viewport.snapshot.end" => return strokes,
            _ => panic!("unexpected snapshot message: {message}"),
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn two_sessions_sync_and_only_owner_can_delete_after_reload(pool: PgPool) {
    let mut server = Server::start(pool.clone(), RealtimeSettings::default()).await;
    let mut author = server.connect(1).await;
    let mut visitor = server.connect(2).await;
    assert!(subscribe(&mut author, "a1").await.is_empty());
    assert!(subscribe(&mut visitor, "b1").await.is_empty());
    let id = Uuid::new_v4();
    let ack = create(&mut author, "r1", stroke(id)).await;
    assert_eq!(ack["request_id"], "r1");
    assert_eq!(ack["operation"], "stroke.create");
    assert_eq!(ack["stroke"]["own"], true);
    assert_eq!(ack["stroke"]["color"], "#AABBCC");
    let own_event = until(&mut author, "stroke.created").await;
    let public_event = until(&mut visitor, "stroke.created").await;
    assert_eq!(own_event["stroke"]["own"], true);
    assert_eq!(public_event["stroke"]["own"], false);
    assert_eq!(public_event["stroke"]["seq"], ack["stroke"]["seq"]);
    assert!(public_event["stroke"].get("owner_digest").is_none());
    assert!(public_event["stroke"].get("session_secret").is_none());

    author.close(None).await.unwrap();
    let mut reloaded = server.connect(1).await;
    let saved = subscribe(&mut reloaded, "reload").await;
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0]["own"], true);
    send(
        &mut visitor,
        json!({"type":"stroke.delete","request_id":"foreign","stroke_id":id}),
    )
    .await;
    let denied = receive(&mut visitor).await;
    assert_eq!(denied["code"], "forbidden");
    assert_eq!(denied["request_id"], "foreign");
    send(
        &mut reloaded,
        json!({"type":"stroke.delete","request_id":"erase","stroke_id":id}),
    )
    .await;
    let deleted = until(&mut reloaded, "ack").await;
    assert_eq!(deleted["already_deleted"], false);
    assert_eq!(deleted["seq"], ack["stroke"]["seq"]);
    assert_eq!(
        until(&mut visitor, "stroke.deleted").await["stroke_id"],
        id.to_string()
    );
    assert_eq!(
        until(&mut reloaded, "stroke.deleted").await["subscription_id"],
        "reload"
    );
    assert!(subscribe(&mut visitor, "after-delete").await.is_empty());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM strokes WHERE deleted_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn retries_conflicts_tombstones_and_offscreen_acks(pool: PgPool) {
    let settings = RealtimeSettings {
        creates_per_second: 20,
        ..Default::default()
    };
    let mut server = Server::start(pool.clone(), settings).await;
    let mut author = server.connect(1).await;
    let mut visitor = server.connect(2).await;
    let id = Uuid::new_v4();
    let mut payload = stroke(id);
    payload["points"] = json!([[10000.845512408225, 10000.100000000002]]);
    assert!(subscribe(&mut author, "origin").await.is_empty());
    let ack = create(&mut author, "create", payload.clone()).await;
    let seq = ack["stroke"]["seq"].clone();
    payload["color"] = json!("#AABBCC");
    assert_eq!(
        create(&mut author, "retry", payload.clone()).await["stroke"]["seq"],
        seq
    );
    send(
        &mut visitor,
        json!({"type":"stroke.create","request_id":"steal","stroke":payload}),
    )
    .await;
    assert_eq!(receive(&mut visitor).await["code"], "id_conflict");
    payload["width"] = json!(7);
    send(
        &mut author,
        json!({"type":"stroke.create","request_id":"change","stroke":payload}),
    )
    .await;
    assert_eq!(receive(&mut author).await["code"], "id_conflict");
    send(
        &mut author,
        json!({"type":"stroke.delete","request_id":"missing","stroke_id":Uuid::new_v4()}),
    )
    .await;
    assert_eq!(receive(&mut author).await["code"], "not_found");
    for (request, already_deleted) in [("delete", false), ("retry-delete", true)] {
        send(
            &mut author,
            json!({"type":"stroke.delete","request_id":request,"stroke_id":id}),
        )
        .await;
        let ack = receive(&mut author).await;
        assert_eq!(ack["type"], "ack");
        assert_eq!(ack["already_deleted"], already_deleted);
        assert_eq!(ack["seq"], seq);
    }
    send(
        &mut visitor,
        json!({"type":"stroke.delete","request_id":"foreign-tombstone","stroke_id":id}),
    )
    .await;
    assert_eq!(receive(&mut visitor).await["code"], "forbidden");
    payload["width"] = json!(6);
    send(
        &mut author,
        json!({"type":"stroke.create","request_id":"resurrect","stroke":payload}),
    )
    .await;
    assert_eq!(receive(&mut author).await["code"], "stroke_deleted");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM strokes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn snapshots_are_byte_bounded_and_replaced_by_new_subscription(pool: PgPool) {
    let settings = RealtimeSettings {
        creates_per_second: 100,
        snapshot_chunk_bytes: 1024,
        snapshot_page_size: 100,
        ..Default::default()
    };
    let mut server = Server::start(pool, settings).await;
    let mut author = server.connect(1).await;
    for index in 0..20 {
        create(&mut author, &format!("r{index}"), stroke(Uuid::new_v4())).await;
    }
    send(&mut author, json!({"type":"viewport.subscribe","subscription_id":"first","bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100}})).await;
    assert_eq!(
        receive(&mut author).await["type"],
        "viewport.snapshot.begin"
    );
    let mut strokes = Vec::new();
    let mut chunks = 0;
    loop {
        let message = receive(&mut author).await;
        assert!(message.to_string().len() <= 1024);
        if message["type"] == "viewport.snapshot.end" {
            break;
        }
        assert_eq!(message["type"], "viewport.snapshot.chunk");
        chunks += 1;
        strokes.extend(message["strokes"].as_array().unwrap().iter().cloned());
    }
    assert!(chunks > 1);
    assert_eq!(strokes.len(), 20);
    assert!(
        strokes
            .windows(2)
            .all(|pair| pair[0]["seq"].as_i64() < pair[1]["seq"].as_i64())
    );
    send(&mut author, json!({"type":"viewport.subscribe","subscription_id":"far","bounds":{"min_x":10000,"min_y":10000,"max_x":11000,"max_y":11000}})).await;
    assert_eq!(
        receive(&mut author).await,
        json!({"type":"viewport.snapshot.begin","subscription_id":"far"})
    );
    assert_eq!(
        receive(&mut author).await,
        json!({"type":"viewport.snapshot.end","subscription_id":"far"})
    );
    send(&mut author, json!({"type":"viewport.subscribe","subscription_id":"loading","bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100}})).await;
    assert_eq!(
        receive(&mut author).await["type"],
        "viewport.snapshot.begin"
    );
    send(&mut author, json!({"type":"viewport.subscribe","subscription_id":"replacement","bounds":{"min_x":10000,"min_y":10000,"max_x":11000,"max_y":11000}})).await;
    let mut replacement_started = false;
    loop {
        let message = receive(&mut author).await;
        if replacement_started {
            assert_eq!(message["subscription_id"], "replacement");
        }
        if message["subscription_id"] != "replacement" {
            continue;
        }
        match message["type"].as_str().unwrap() {
            "viewport.snapshot.begin" => replacement_started = true,
            "viewport.snapshot.end" => {
                assert!(replacement_started);
                break;
            }
            _ => panic!("unexpected replacement message: {message}"),
        }
    }
    create(&mut author, "after-cancel", stroke(Uuid::new_v4())).await;
    assert_eq!(subscribe(&mut author, "back").await.len(), 21);
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn mutations_after_snapshot_begin_are_delivered_after_snapshot_end(pool: PgPool) {
    let settings = RealtimeSettings {
        creates_per_second: 100,
        snapshot_page_size: 1,
        ..Default::default()
    };
    let mut server = Server::start(pool, settings).await;
    let mut author = server.connect(1).await;
    let removed = Uuid::new_v4();
    create(&mut author, "seed", stroke(removed)).await;
    for index in 0..30 {
        create(&mut author, &format!("s{index}"), stroke(Uuid::new_v4())).await;
    }
    let mut visitor = server.connect(2).await;
    send(&mut visitor, json!({"type":"viewport.subscribe","subscription_id":"race","bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100}})).await;
    assert_eq!(
        receive(&mut visitor).await["type"],
        "viewport.snapshot.begin"
    );
    let created = Uuid::new_v4();
    create(&mut author, "during", stroke(created)).await;
    send(
        &mut author,
        json!({"type":"stroke.delete","request_id":"during-delete","stroke_id":removed}),
    )
    .await;
    assert_eq!(receive(&mut author).await["type"], "ack");
    let mut ended = false;
    let mut saw_create = false;
    let mut saw_delete = false;
    let mut snapshot_ids = Vec::new();
    while !ended || !saw_create || !saw_delete {
        let message = receive(&mut visitor).await;
        assert_eq!(message["subscription_id"], "race");
        match message["type"].as_str().unwrap() {
            "viewport.snapshot.chunk" => {
                assert!(!ended);
                snapshot_ids.extend(
                    message["strokes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|stroke| stroke["id"].as_str().unwrap().to_owned()),
                );
            }
            "viewport.snapshot.end" => ended = true,
            "stroke.created" => {
                assert!(ended);
                assert_eq!(message["stroke"]["id"], created.to_string());
                saw_create = true;
            }
            "stroke.deleted" => {
                assert!(ended);
                assert_eq!(message["stroke_id"], removed.to_string());
                saw_delete = true;
            }
            _ => panic!("unexpected race message: {message}"),
        }
    }
    assert_eq!(snapshot_ids.len(), 31);
    assert!(snapshot_ids.contains(&removed.to_string()));
    assert!(!snapshot_ids.contains(&created.to_string()));
    let after_reconnect = subscribe(&mut visitor, "resync").await;
    assert_eq!(after_reconnect.len(), 31);
    assert!(
        !after_reconnect
            .iter()
            .any(|stroke| stroke["id"] == removed.to_string())
    );
    assert!(
        after_reconnect
            .iter()
            .any(|stroke| stroke["id"] == created.to_string())
    );
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn rejects_bad_origin_unauthenticated_and_invalid_input_and_limits_rate(pool: PgPool) {
    let settings = RealtimeSettings {
        creates_per_second: 1,
        ..Default::default()
    };
    let mut server = Server::start(pool.clone(), settings).await;
    for origin in [None, Some("https://evil.example")] {
        let mut request = server.url.clone().into_client_request().unwrap();
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("Origin", origin.parse().unwrap());
        }
        let error = connect_async(request).await.err().unwrap();
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("expected HTTP rejection");
        };
        assert_eq!(response.status(), 403);
    }
    let mut request = server.url.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    let (mut unauthenticated, _) = connect_async(request.clone()).await.unwrap();
    send(
        &mut unauthenticated,
        json!({"type":"stroke.delete","request_id":"nohello","stroke_id":Uuid::new_v4()}),
    )
    .await;
    assert_eq!(
        receive(&mut unauthenticated).await["code"],
        "hello_required"
    );
    let (mut invalid_session, _) = connect_async(request).await.unwrap();
    send(
        &mut invalid_session,
        json!({"type":"hello","session_secret":"invalid"}),
    )
    .await;
    assert_eq!(
        receive(&mut invalid_session).await["code"],
        "invalid_session"
    );
    let mut author = server.connect(1).await;
    send(&mut author, json!({"type":"viewport.subscribe","subscription_id":"huge","bounds":{"min_x":-1e9,"min_y":-1e9,"max_x":1e9,"max_y":1e9}})).await;
    assert_eq!(receive(&mut author).await["code"], "validation_failed");
    send(
        &mut author,
        json!({"type":"stroke.create","request_id":"bad","stroke":{"seq":1}}),
    )
    .await;
    let bad = receive(&mut author).await;
    assert_eq!(bad["code"], "invalid_message");
    assert_eq!(bad["request_id"], "bad");
    create(&mut author, "one", stroke(Uuid::new_v4())).await;
    send(
        &mut author,
        json!({"type":"stroke.create","request_id":"two","stroke":stroke(Uuid::new_v4())}),
    )
    .await;
    assert_eq!(receive(&mut author).await["code"], "rate_limited");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM strokes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn bounds_connections_payload_and_hello_wait(pool: PgPool) {
    let settings = RealtimeSettings {
        max_connections: 1,
        max_message_bytes: 1024,
        hello_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let mut server = Server::start(pool.clone(), settings).await;
    let mut socket = server.connect(1).await;
    let mut request = server.url.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    let error = connect_async(request.clone()).await.err().unwrap();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected capacity rejection");
    };
    assert_eq!(response.status(), 503);
    socket
        .send(Message::Text("x".repeat(2048).into()))
        .await
        .unwrap();
    let frame = timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap();
    assert!(matches!(
        frame,
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
    // Wait for the closed socket's permit to be returned before checking the hello deadline.
    let mut idle = None;
    for _ in 0..50 {
        if let Ok((socket, _)) = connect_async(request.clone()).await {
            idle = Some(socket);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut idle = idle.expect("connection permit was not released");
    let frame = timeout(Duration::from_secs(2), idle.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(frame, Message::Close(_)));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM strokes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn chisel_and_spray_roundtrip_with_retry_and_snapshot(pool: PgPool) {
    let server = Server::start(pool.clone(), RealtimeSettings::default()).await;
    let mut author = server.connect(1).await;
    let mut visitor = server.connect(2).await;
    assert!(subscribe(&mut author, "a1").await.is_empty());
    assert!(subscribe(&mut visitor, "b1").await.is_empty());
    let mut seqs = Vec::new();
    for (index, brush) in ["chisel", "spray"].into_iter().enumerate() {
        let id = Uuid::new_v4();
        let mut payload = stroke(id);
        payload["brush"] = json!(brush);
        payload["width"] = json!(64);
        let ack = create(&mut author, &format!("c{index}"), payload.clone()).await;
        assert_eq!(ack["stroke"]["brush"], brush);
        let points: Vec<[f64; 2]> =
            serde_json::from_value(ack["stroke"]["points"].clone()).unwrap();
        assert_eq!(points, vec![[0.0, 0.0], [10.0, 5.0]]);
        let event = until(&mut visitor, "stroke.created").await;
        assert_eq!(event["stroke"]["brush"], brush);
        assert_eq!(event["stroke"]["own"], false);
        // A retry with the same UUID/content returns the same object and seq.
        let retry = create(&mut author, &format!("retry{index}"), payload.clone()).await;
        assert_eq!(retry["stroke"]["seq"], ack["stroke"]["seq"]);
        // Same UUID with another brush is a conflict, never a silent rewrite.
        // Checked once: five creates stay inside the 5/s create limit.
        if index == 0 {
            payload["brush"] = json!("round");
            send(
                &mut author,
                json!({"type":"stroke.create","request_id":format!("conflict{index}"),"stroke":payload}),
            )
            .await;
            let mut conflict = receive(&mut author).await;
            while conflict["type"] != "error" {
                conflict = receive(&mut author).await;
            }
            assert_eq!(conflict["code"], "id_conflict");
        }
        seqs.push(ack["stroke"]["seq"].clone());
    }
    author.close(None).await.unwrap();
    let mut reloaded = server.connect(1).await;
    let saved = subscribe(&mut reloaded, "reload").await;
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0]["brush"], "chisel");
    assert_eq!(saved[1]["brush"], "spray");
    assert_eq!(saved[0]["seq"], seqs[0]);
    assert!(saved.iter().all(|stroke| stroke["own"] == true));
}
