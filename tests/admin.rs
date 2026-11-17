//! Real WebAuthn cryptography with ephemeral software authenticators, never a production bypass.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Method, Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use endless_drawing_server::{
    admin::{self, AdminConfig, AdminService, provision},
    app,
    config::RealtimeSettings,
    domain::StrokeLimits,
    realtime::Realtime,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::task::AbortOnDropHandle;
use tower::ServiceExt;
use uuid::Uuid;
use webauthn_authenticator_rs::{prelude::WebauthnAuthenticator, softpasskey::SoftPasskey};
use webauthn_rs::prelude::*;
use zeroize::Zeroizing;

#[path = "support/passkey.rs"]
mod passkey;
use passkey::{BE, BS, TestPasskey, UP, UV};
#[path = "support/admin_totp.rs"]
mod totp_tests;

const ORIGIN: &str = "https://canvas.example.test";
const PASSWORD: &str = "test-only manager generated password 42!";
const LOGIN: &str = "moderator";

struct Fixture {
    pool: PgPool,
    router: axum::Router,
    realtime: Realtime,
    worker: AbortOnDropHandle<()>,
    http: AbortOnDropHandle<()>,
    ws: String,
    primary: SoftPasskey,
    backup: SoftPasskey,
}

impl Fixture {
    async fn new(pool: PgPool) -> Self {
        let primary = SoftPasskey::new(true);
        let backup = SoftPasskey::new(true);
        let settings = RealtimeSettings {
            allowed_origins: vec![ORIGIN.into()],
            // Leave room for deliberate DB-lock/revocation tests on slow CI workers.
            operation_timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let (realtime, worker) =
            Realtime::start(pool.clone(), StrokeLimits::default(), settings).unwrap();
        let config = AdminConfig::new(ORIGIN, "example.test".into())
            .unwrap()
            .with_totp_key([7; 32]);
        let service = AdminService::new(pool.clone(), &realtime, config)
            .await
            .unwrap();
        let router = app::router(pool.clone(), Duration::from_secs(1), None)
            .merge(realtime.router())
            .merge(admin::router(Some(service), None));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = format!("ws://{}/ws", listener.local_addr().unwrap());
        let server = router.clone();
        let shutdown = realtime.shutdown_token();
        let http = tokio::spawn(async move {
            axum::serve(
                listener,
                server.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
        });
        Self {
            pool,
            router,
            realtime,
            worker: AbortOnDropHandle::new(worker),
            http: AbortOnDropHandle::new(http),
            ws,
            primary,
            backup,
        }
    }

    async fn enrolled(&mut self) -> Client {
        let grant = provision::create(&self.pool, LOGIN, Zeroizing::new(PASSWORD.into()))
            .await
            .unwrap();
        let mut client = Client::default();
        client.options(self).await;
        for primary in [true, false] {
            let (status, challenge, _) = client.call(self, Method::POST, "/api/admin/enrollment/start", Some(json!({"login":LOGIN,"password":PASSWORD,"enrollment_token":grant.secret(),"label":if primary {"Primary"} else {"Backup"}}))).await;
            assert_eq!(status, StatusCode::OK, "{challenge}");
            assert_eq!(challenge["webauthn"]["publicKey"]["attestation"], "none");
            assert_eq!(
                challenge["webauthn"]["publicKey"]["authenticatorSelection"]["userVerification"],
                "required"
            );
            let request = serde_json::from_value(challenge["webauthn"].clone()).unwrap();
            let key = if primary {
                &mut self.primary
            } else {
                &mut self.backup
            };
            let credential = key
                .do_registration(Url::parse(ORIGIN).unwrap(), request)
                .unwrap();
            let (status, result, _) = client
                .call(
                    self,
                    Method::POST,
                    "/api/admin/enrollment/finish",
                    Some(json!({"credential":credential})),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{result}");
            assert_eq!(result["complete"], true);
            assert_eq!(result["required_credentials"], 1);
        }
        // Enrollment is not authentication.
        assert_eq!(
            client
                .call(self, Method::GET, "/api/admin/session", None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        client
    }

    async fn login(&mut self, client: &mut Client, backup: bool) -> Value {
        client.options(self).await;
        let (status, challenge, _) = client
            .call(
                self,
                Method::POST,
                "/api/admin/login",
                Some(json!({"login":LOGIN,"password":PASSWORD})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{challenge}");
        assert_eq!(challenge["status"], "mfa_required");
        assert_eq!(
            challenge["webauthn"]["publicKey"]["userVerification"],
            "required"
        );
        let request = serde_json::from_value(challenge["webauthn"].clone()).unwrap();
        let key = if backup {
            &mut self.backup
        } else {
            &mut self.primary
        };
        let credential = key
            .do_authentication(Url::parse(ORIGIN).unwrap(), request)
            .unwrap();
        let (status, result, headers) = client
            .call(
                self,
                Method::POST,
                "/api/admin/login/verify",
                Some(json!({"credential":credential})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["authenticated"], true);
        assert!(headers.get_all("set-cookie").iter().any(|header| {
            let value = header.to_str().unwrap();
            value.starts_with("__Host-ed-admin=")
                && value.contains("HttpOnly")
                && value.contains("Secure")
                && value.contains("SameSite=Strict")
                && value.contains("Path=/")
                && !value.contains("Domain=")
        }));
        result
    }

    async fn stop(&mut self) {
        self.realtime.shutdown_token().cancel();
        tokio::time::timeout(Duration::from_secs(5), &mut self.worker)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut self.http)
            .await
            .unwrap()
            .unwrap();
    }

    async fn socket(&self, seed: u8) -> Socket {
        self.socket_with_client(seed, None).await
    }

    async fn socket_with_client(&self, seed: u8, client: Option<&Client>) -> Socket {
        let mut request = self.ws.clone().into_client_request().unwrap();
        if let Some(client) = client {
            request.headers_mut().insert(
                "cookie",
                client
                    .cookies
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join("; ")
                    .parse()
                    .unwrap(),
            );
        }
        request
            .headers_mut()
            .insert("origin", ORIGIN.parse().unwrap());
        let (mut socket, _) = connect_async(request).await.unwrap();
        ws_send(
            &mut socket,
            json!({"type":"hello","session_secret":URL_SAFE_NO_PAD.encode([seed;32])}),
        )
        .await;
        assert_eq!(ws_receive(&mut socket).await["type"], "hello.ack");
        ws_snapshot(&mut socket, "v1").await;
        socket
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.realtime.shutdown_token().cancel();
    }
}

#[derive(Default, Clone)]
struct Client {
    cookies: BTreeMap<String, String>,
    csrf: Option<String>,
}
impl Client {
    async fn call(
        &mut self,
        fixture: &Fixture,
        method: Method,
        path: &str,
        payload: Option<Value>,
    ) -> (StatusCode, Value, HeaderMap) {
        self.call_origin(fixture, method, path, payload, ORIGIN)
            .await
    }
    async fn call_origin(
        &mut self,
        fixture: &Fixture,
        method: Method,
        path: &str,
        payload: Option<Value>,
        origin: &str,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("origin", origin)
            .extension(ConnectInfo(
                "127.0.0.1:30000".parse::<SocketAddr>().unwrap(),
            ));
        if !self.cookies.is_empty() {
            request = request.header(
                "cookie",
                self.cookies
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        if let Some(csrf) = &self.csrf {
            request = request.header("x-csrf-token", csrf);
        }
        let body = payload
            .map(|payload| payload.to_string())
            .unwrap_or_default();
        let response = fixture
            .router
            .clone()
            .oneshot(
                request
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["x-frame-options"], "DENY");
        for cookie in headers.get_all("set-cookie") {
            let first = cookie.to_str().unwrap().split(';').next().unwrap();
            let (name, value) = first.split_once('=').unwrap();
            if value.is_empty() {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.into(), value.into());
            }
        }
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let result: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        if let Some(csrf) = result.get("csrf_token").and_then(Value::as_str) {
            self.csrf = Some(csrf.into());
        }
        (status, result, headers)
    }
    async fn options(&mut self, fixture: &Fixture) {
        let (status, result, _) = self
            .call(fixture, Method::GET, "/api/admin/login/options", None)
            .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["authenticator_policy"], "passkey_uv");
    }

    async fn start_login(&mut self, fixture: &Fixture) -> Value {
        self.options(fixture).await;
        let (status, response, _) = self
            .call(
                fixture,
                Method::POST,
                "/api/admin/login",
                Some(json!({"login":LOGIN,"password":PASSWORD})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        response["webauthn"].clone()
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
async fn ws_send(socket: &mut Socket, message: Value) {
    socket
        .send(Message::Text(message.to_string().into()))
        .await
        .unwrap();
}
async fn ws_receive(socket: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(value) => socket.send(Message::Pong(value)).await.unwrap(),
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    })
    .await
    .unwrap()
}
async fn ws_snapshot(socket: &mut Socket, subscription: &str) -> Vec<Value> {
    ws_send(socket, json!({"type":"viewport.subscribe","subscription_id":subscription,"bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100}})).await;
    assert_eq!(ws_receive(socket).await["type"], "viewport.snapshot.begin");
    let mut strokes = Vec::new();
    loop {
        let message = ws_receive(socket).await;
        match message["type"].as_str().unwrap() {
            "viewport.snapshot.chunk" => {
                strokes.extend(message["strokes"].as_array().unwrap().iter().cloned())
            }
            "viewport.snapshot.end" => return strokes,
            _ => panic!("unexpected snapshot message: {message}"),
        }
    }
}

async fn ws_create(socket: &mut Socket, id: Uuid) {
    ws_send(socket, json!({"type":"stroke.create","request_id":"create","stroke":{"id":id,"points":[[0,0],[10,5]],"brush":"round","width":6,"color":"#202020","opacity":1}})).await;
    assert_eq!(ws_receive(socket).await["type"], "ack");
    assert_eq!(ws_receive(socket).await["type"], "stroke.created");
}
fn hide(id: Uuid, ids: Vec<Uuid>, reason: &str) -> Value {
    json!({"operation_id":id,"stroke_ids":ids,"reason":reason})
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn password_is_insufficient_both_keys_work_and_logout_revokes(pool: PgPool) {
    let mut fixture = Fixture::new(pool).await;
    let mut client = fixture.enrolled().await;
    let (status, challenge, _) = client
        .call(
            &fixture,
            Method::POST,
            "/api/admin/login",
            Some(json!({"login":LOGIN,"password":PASSWORD})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    assert!(!client.cookies.contains_key("__Host-ed-admin"));
    assert_eq!(
        client
            .call(&fixture, Method::GET, "/api/admin/session", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(hide(Uuid::new_v4(), vec![Uuid::new_v4()], "reason"))
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    fixture.login(&mut client, false).await;
    let primary_session = client.cookies["__Host-ed-admin"].clone();
    let mut captured = client.clone();
    assert_eq!(
        client
            .call(&fixture, Method::POST, "/api/admin/logout", Some(json!({})))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        captured
            .call(&fixture, Method::GET, "/api/admin/session", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    fixture.login(&mut client, true).await;
    assert_ne!(primary_session, client.cookies["__Host-ed-admin"]);
    assert_eq!(
        client
            .call(&fixture, Method::GET, "/api/admin/session", None)
            .await
            .0,
        StatusCode::OK
    );
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn invalid_assertion_origin_expiry_and_replay_never_mint_sessions(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut client = fixture.enrolled().await;
    for mode in ["signature", "origin", "expiry", "success"] {
        client.options(&fixture).await;
        let (status, challenge, _) = client
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login",
                Some(json!({"login":LOGIN,"password":PASSWORD})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{challenge}");
        let origin = if mode == "origin" {
            "https://other.example.test"
        } else {
            ORIGIN
        };
        let mut credential = fixture
            .primary
            .do_authentication(
                Url::parse(origin).unwrap(),
                serde_json::from_value(challenge["webauthn"].clone()).unwrap(),
            )
            .unwrap();
        if mode == "signature" {
            credential.response.signature[0] ^= 1;
        }
        if mode == "expiry" {
            sqlx::query(
                "UPDATE admin_preauth SET challenge_expires_at = TIMESTAMPTZ '2000-01-01 00:00:00+00'",
            )
            .execute(&pool)
            .await
            .unwrap();
        }
        let body = json!({"credential":credential});
        let mut replay = client.clone();
        let (status, response, _) = client
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login/verify",
                Some(body.clone()),
            )
            .await;
        assert_eq!(
            status,
            if mode == "success" {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            },
            "{response}"
        );
        assert_eq!(
            replay
                .call(
                    &fixture,
                    Method::POST,
                    "/api/admin/login/verify",
                    Some(body)
                )
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn hide_is_atomic_audited_idempotent_and_broadcasts_without_ownership_bypass(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut admin = fixture.enrolled().await;
    fixture.login(&mut admin, false).await;
    let mut author = fixture.socket(1).await;
    // Even a valid administrator cookie must not elevate public WS deletion rights.
    let mut visitor = fixture.socket_with_client(2, Some(&admin)).await;
    let id = Uuid::new_v4();
    ws_create(&mut author, id).await;
    assert_eq!(ws_receive(&mut visitor).await["stroke"]["own"], false);
    ws_send(
        &mut visitor,
        json!({"type":"stroke.delete","request_id":"foreign","stroke_id":id}),
    )
    .await;
    assert_eq!(ws_receive(&mut visitor).await["code"], "forbidden");
    let operation = Uuid::new_v4();
    let unknown = hide(operation, vec![id, Uuid::new_v4()], "reason");
    assert_eq!(
        admin
            .call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(unknown)
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM strokes WHERE deleted_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    let payload = hide(operation, vec![id], " reason ");
    let (status, result, _) = admin
        .call(
            &fixture,
            Method::POST,
            "/api/admin/strokes/hide",
            Some(payload.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["hidden"], json!([id]));
    for socket in [&mut author, &mut visitor] {
        let event = ws_receive(socket).await;
        assert_eq!(event["type"], "stroke.deleted");
        assert_eq!(event["stroke_id"], id.to_string());
        assert!(event.get("reason").is_none() && event.get("actor_id").is_none());
    }
    assert_eq!(
        admin
            .call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(payload)
            )
            .await
            .1,
        result
    );
    assert_eq!(
        admin
            .call(
                &fixture,
                Method::GET,
                &format!("/api/admin/operations/{operation}"),
                None
            )
            .await
            .1,
        result
    );
    assert_eq!(
        admin
            .call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(hide(operation, vec![id], "changed"))
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_security_audit WHERE event = 'strokes_hidden'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let (_, result, _) = admin
        .call(
            &fixture,
            Method::POST,
            "/api/admin/strokes/hide",
            Some(hide(Uuid::new_v4(), vec![id], "already")),
        )
        .await;
    assert_eq!(result["already_hidden"], json!([id]));
    ws_send(&mut author, json!({"type":"stroke.create","request_id":"recreate-hidden","stroke":{"id":id,"points":[[0,0],[10,5]],"brush":"round","width":6,"color":"#202020","opacity":1}})).await;
    assert_eq!(ws_receive(&mut author).await["code"], "stroke_deleted");
    ws_send(
        &mut author,
        json!({"type":"stroke.delete","request_id":"delete-hidden","stroke_id":id}),
    )
    .await;
    let deleted = ws_receive(&mut author).await;
    assert_eq!(deleted["type"], "ack", "{deleted}");
    assert_eq!(deleted["already_deleted"], true);
    let sentinel = Uuid::new_v4();
    ws_create(&mut author, sentinel).await;
    assert_eq!(
        ws_receive(&mut visitor).await["type"],
        "stroke.created",
        "retries did not publish duplicate deletes"
    );
    for socket in [&mut author, &mut visitor] {
        let snapshot = ws_snapshot(socket, "after-hide").await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0]["id"], sentinel.to_string());
    }
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn csrf_origin_audit_failure_and_recovery_fail_closed(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut admin = fixture.enrolled().await;
    fixture.login(&mut admin, false).await;
    let mut author = fixture.socket(1).await;
    let id = Uuid::new_v4();
    ws_create(&mut author, id).await;
    let payload = hide(Uuid::new_v4(), vec![id], "reason");
    assert_eq!(
        admin
            .call_origin(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(payload.clone()),
                "https://evil.example.test"
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let old_csrf = admin.csrf.take();
    assert_eq!(
        admin
            .call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(payload.clone())
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    admin.csrf = old_csrf;
    sqlx::query("ALTER TABLE admin_security_audit ADD CONSTRAINT test_audit_failure CHECK (event <> 'strokes_hidden')").execute(&pool).await.unwrap();
    assert_eq!(
        admin
            .call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(payload.clone())
            )
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM strokes WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!deleted);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("ALTER TABLE admin_security_audit DROP CONSTRAINT test_audit_failure")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        admin
            .call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(payload)
            )
            .await
            .0,
        StatusCode::OK
    );
    let _recovery = provision::recover(
        &pool,
        LOGIN,
        Zeroizing::new("new test-only password manager value 44!".into()),
    )
    .await
    .unwrap();
    assert_eq!(
        admin
            .call(&fixture, Method::GET, "/api/admin/session", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admin_credentials WHERE revoked_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn idle_and_absolute_expiry_disable_and_password_rate_limits(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut admin = fixture.enrolled().await;
    fixture.login(&mut admin, false).await;
    sqlx::query(
        "UPDATE admin_sessions SET last_seen_at = clock_timestamp() - interval '16 minutes'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        admin
            .call(&fixture, Method::GET, "/api/admin/session", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    fixture.login(&mut admin, true).await;
    sqlx::query("UPDATE admin_sessions SET expires_at = TIMESTAMPTZ '2000-01-01 00:00:00+00'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        admin
            .call(&fixture, Method::GET, "/api/admin/session", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    provision::disable(&pool, LOGIN).await.unwrap();
    admin.options(&fixture).await;
    let mut saw_limit = false;
    for _ in 0..8 {
        let (status, result, _) = admin
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login",
                Some(json!({"login":LOGIN,"password":PASSWORD})),
            )
            .await;
        assert!(
            matches!(
                status,
                StatusCode::UNAUTHORIZED | StatusCode::TOO_MANY_REQUESTS
            ),
            "{result}"
        );
        saw_limit |= status == StatusCode::TOO_MANY_REQUESTS;
    }
    assert!(saw_limit);
    fixture.stop().await;
}

async fn enroll_test_passkeys(fixture: &Fixture, flags: u8) -> (Client, TestPasskey) {
    let grant = provision::create(&fixture.pool, LOGIN, Zeroizing::new(PASSWORD.into()))
        .await
        .unwrap();
    let mut client = Client::default();
    client.options(fixture).await;
    let primary = TestPasskey::new();
    let backup = TestPasskey::new();
    for (index, key) in [&primary, &backup].into_iter().enumerate() {
        let (status, challenge, _) = client.call(fixture, Method::POST, "/api/admin/enrollment/start", Some(json!({"login":LOGIN,"password":PASSWORD,"enrollment_token":grant.secret(),"label":format!("Passkey {index}")}))).await;
        assert_eq!(status, StatusCode::OK, "{challenge}");
        let credential = key.register(&challenge["webauthn"], ORIGIN, flags);
        let (status, result, _) = client
            .call(
                fixture,
                Method::POST,
                "/api/admin/enrollment/finish",
                Some(json!({"credential":credential})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["complete"], true);
    }
    (client, primary)
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn synced_passkeys_accept_zero_counters_and_persist_backup_state(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let (mut client, key) = enroll_test_passkeys(&fixture, UP | UV | BE | BS).await;
    for backed_up in [false, true] {
        let options = client.start_login(&fixture).await;
        let flags = UP | UV | BE | if backed_up { BS } else { 0 };
        let credential = key.authenticate(&options, ORIGIN, flags, 0);
        let (status, result, _) = client
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login/verify",
                Some(json!({"credential":credential})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        let (counter, stored): (i64, sqlx::types::Json<Passkey>) = sqlx::query_as(
            "SELECT counter, credential FROM admin_credentials WHERE credential_id = $1",
        )
        .bind(URL_SAFE_NO_PAD.decode(key.id()).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
        let credential: Credential = stored.0.into();
        assert_eq!(counter, 0);
        assert!(credential.backup_eligible);
        assert_eq!(credential.backup_state, backed_up);
    }
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn correctly_signed_assertions_still_require_uv_rp_and_challenge(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let (mut client, key) = enroll_test_passkeys(&fixture, UP | UV | BE | BS).await;
    for mode in ["uv", "rp", "challenge", "user_handle"] {
        let mut options = client.start_login(&fixture).await;
        if mode == "rp" {
            options["publicKey"]["rpId"] = json!("other.example.test");
        }
        if mode == "challenge" {
            options["publicKey"]["challenge"] = json!(URL_SAFE_NO_PAD.encode([9; 32]));
        }
        let flags = UP | BE | BS | if mode == "uv" { 0 } else { UV };
        let mut credential = key.authenticate(&options, ORIGIN, flags, 0);
        if mode == "user_handle" {
            credential["response"]["userHandle"] =
                json!(URL_SAFE_NO_PAD.encode(Uuid::new_v4().as_bytes()));
        }
        let (status, result, _) = client
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login/verify",
                Some(json!({"credential":credential})),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{mode}: {result}");
        assert!(!client.cookies.contains_key("__Host-ed-admin"));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn concurrent_verification_consumes_one_challenge_and_mints_one_session(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut first = fixture.enrolled().await;
    let options = first.start_login(&fixture).await;
    let credential = fixture
        .primary
        .do_authentication(
            Url::parse(ORIGIN).unwrap(),
            serde_json::from_value(options).unwrap(),
        )
        .unwrap();
    let body = json!({"credential":credential});
    let mut second = first.clone();
    let (a, b) = tokio::join!(
        first.call(
            &fixture,
            Method::POST,
            "/api/admin/login/verify",
            Some(body.clone())
        ),
        second.call(
            &fixture,
            Method::POST,
            "/api/admin/login/verify",
            Some(body)
        )
    );
    assert!(
        [StatusCode::OK, StatusCode::UNAUTHORIZED].contains(&a.0),
        "{}",
        a.1
    );
    assert!(
        [StatusCode::OK, StatusCode::UNAUTHORIZED].contains(&b.0),
        "{}",
        b.1
    );
    assert_ne!(a.0, b.0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn device_bound_counters_are_checked_against_current_database_not_challenge_snapshot(
    pool: PgPool,
) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut older = fixture.enrolled().await;
    let first = older.start_login(&fixture).await;
    let first = fixture
        .primary
        .do_authentication(
            Url::parse(ORIGIN).unwrap(),
            serde_json::from_value(first).unwrap(),
        )
        .unwrap();
    let mut newer = Client::default();
    let second = newer.start_login(&fixture).await;
    let second = fixture
        .primary
        .do_authentication(
            Url::parse(ORIGIN).unwrap(),
            serde_json::from_value(second).unwrap(),
        )
        .unwrap();
    assert_eq!(
        newer
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login/verify",
                Some(json!({"credential":second}))
            )
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        older
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login/verify",
                Some(json!({"credential":first}))
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    fixture.stop().await;
}

async fn queued_hide_rechecks_revocation(pool: PgPool, recover: bool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut admin = fixture.enrolled().await;
    fixture.login(&mut admin, false).await;
    let mut author = fixture.socket(1).await;
    let blocker = Uuid::new_v4();
    let victim = Uuid::new_v4();
    ws_create(&mut author, blocker).await;
    ws_create(&mut author, victim).await;
    // Hold a public mutation ahead of hide in the one writer, without locking the account.
    let mut lock = pool.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM strokes WHERE id = $1 FOR UPDATE")
        .bind(blocker)
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    ws_send(
        &mut author,
        json!({"type":"stroke.delete","request_id":"blocker","stroke_id":blocker}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND $1 = ANY(pg_blocking_pids(pid)))").bind(blocker_pid).fetch_one(&pool).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let baseline: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("UPDATE admin_sessions SET last_seen_at = clock_timestamp() - interval '1 minute' RETURNING last_seen_at").fetch_one(&pool).await.unwrap();
    let mut pending = admin.clone();
    let operation = Uuid::new_v4();
    let (response, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(
            pending.call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(hide(operation, vec![victim], "queued"))
            ),
            async {
                // Prove HTTP preflight accepted the full session before revocation.
                loop {
                    let authorized: bool =
                        sqlx::query_scalar("SELECT last_seen_at > $1 FROM admin_sessions")
                            .bind(baseline)
                            .fetch_one(&pool)
                            .await
                            .unwrap();
                    if authorized {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if recover {
                    provision::recover(
                        &pool,
                        LOGIN,
                        Zeroizing::new("new test-only generated password 45!".into()),
                    )
                    .await
                    .unwrap();
                } else {
                    assert_eq!(
                        admin
                            .call(&fixture, Method::POST, "/api/admin/logout", Some(json!({})))
                            .await
                            .0,
                        StatusCode::NO_CONTENT
                    );
                }
                lock.commit().await.unwrap();
            }
        )
    })
    .await
    .unwrap();
    assert_eq!(response.0, StatusCode::UNAUTHORIZED, "{}", response.1);
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM strokes WHERE id = $1")
            .bind(victim)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!deleted);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_operations WHERE id = $1")
        .bind(operation)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn logout_revokes_a_hide_accepted_by_http_but_waiting_for_the_writer(pool: PgPool) {
    queued_hide_rechecks_revocation(pool, false).await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn recovery_revokes_a_hide_accepted_by_http_but_waiting_for_the_writer(pool: PgPool) {
    queued_hide_rechecks_revocation(pool, true).await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn commit_failure_invalidates_live_views_and_retry_recovers_from_a_fresh_snapshot(
    pool: PgPool,
) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut admin = fixture.enrolled().await;
    fixture.login(&mut admin, false).await;
    let mut author = fixture.socket(1).await;
    let mut visitor = fixture.socket(2).await;
    let id = Uuid::new_v4();
    ws_create(&mut author, id).await;
    assert_eq!(ws_receive(&mut visitor).await["type"], "stroke.created");
    // A deferred failure exercises COMMIT, not the earlier audit/UPDATE error path.
    sqlx::raw_sql("CREATE FUNCTION test_reject_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test-only deferred failure'; END $$;
        CREATE CONSTRAINT TRIGGER test_commit_failure AFTER INSERT ON moderation_operations DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION test_reject_commit();")
        .execute(&pool).await.unwrap();
    let operation = Uuid::new_v4();
    let payload = hide(operation, vec![id], "commit failure");
    let (status, result, _) = admin
        .call(
            &fixture,
            Method::POST,
            "/api/admin/strokes/hide",
            Some(payload.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{result}");
    assert_eq!(result["error"]["code"], "temporarily_unavailable");
    for socket in [&mut author, &mut visitor] {
        let event = ws_receive(socket).await;
        assert_eq!(event["type"], "error");
        assert_eq!(event["code"], "resync_required");
    }
    assert_eq!(
        admin
            .call(
                &fixture,
                Method::GET,
                &format!("/api/admin/operations/{operation}"),
                None
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    sqlx::raw_sql("DROP TRIGGER test_commit_failure ON moderation_operations; DROP FUNCTION test_reject_commit();").execute(&pool).await.unwrap();
    for socket in [&mut author, &mut visitor] {
        let snapshot = ws_snapshot(socket, "after-commit-failure").await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0]["id"], id.to_string());
    }
    assert_eq!(
        admin
            .call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(payload)
            )
            .await
            .0,
        StatusCode::OK
    );
    for socket in [&mut author, &mut visitor] {
        assert_eq!(ws_receive(socket).await["type"], "stroke.deleted");
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_security_audit WHERE event = 'strokes_hidden'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn enrollment_requires_uv_a_live_grant_and_one_passkey_before_login(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let grant = provision::create(&pool, LOGIN, Zeroizing::new(PASSWORD.into()))
        .await
        .unwrap();
    let mut client = Client::default();
    client.options(&fixture).await;
    let key = TestPasskey::new();
    let backup = TestPasskey::new();
    for mode in ["uv", "first", "expired"] {
        let (status, challenge, _) = client.call(&fixture, Method::POST, "/api/admin/enrollment/start", Some(json!({"login":LOGIN,"password":PASSWORD,"enrollment_token":grant.secret(),"label":mode}))).await;
        assert_eq!(status, StatusCode::OK, "{challenge}");
        if mode == "expired" {
            // Avoid a one-second boundary: container/host clock corrections during builds
            // must not accidentally make an intentionally expired fixture live again.
            let expired: bool = sqlx::query_scalar(
                "UPDATE admin_enrollments SET expires_at = TIMESTAMPTZ '2000-01-01 00:00:00+00' RETURNING expires_at < clock_timestamp()",
            ).fetch_one(&pool).await.unwrap();
            assert!(expired);
        }
        let flags = UP | BE | BS | if mode == "uv" { 0 } else { UV };
        let registration_key = if mode == "expired" { &backup } else { &key };
        let credential = registration_key.register(&challenge["webauthn"], ORIGIN, flags);
        let (status, result, _) = client
            .call(
                &fixture,
                Method::POST,
                "/api/admin/enrollment/finish",
                Some(json!({"credential":credential})),
            )
            .await;
        assert_eq!(
            status,
            if mode == "first" {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            },
            "{mode}: {result}"
        );
        if mode == "first" {
            assert_eq!(result["complete"], true);
            assert_eq!(
                client
                    .call(
                        &fixture,
                        Method::POST,
                        "/api/admin/login",
                        Some(json!({"login":LOGIN,"password":PASSWORD}))
                    )
                    .await
                    .0,
                StatusCode::OK
            );
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_credentials")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fixture.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn admin_http_guards_reject_login_logout_csrf_cross_site_and_oversized_payloads(
    pool: PgPool,
) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut client = fixture.enrolled().await;
    let preauth_csrf = client.csrf.clone();
    client.csrf = None;
    assert_eq!(
        client
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login",
                Some(json!({"login":LOGIN,"password":PASSWORD}))
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    client.csrf = preauth_csrf.clone();
    fixture.login(&mut client, false).await;
    assert_eq!(
        client
            .call_origin(
                &fixture,
                Method::GET,
                "/api/admin/session",
                None,
                "https://evil.example.test"
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut cross_site = Request::get("/api/admin/session")
        .header("sec-fetch-site", "cross-site")
        .header(
            "cookie",
            format!("__Host-ed-admin={}", client.cookies["__Host-ed-admin"]),
        )
        .body(Body::empty())
        .unwrap();
    cross_site.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:30000".parse::<SocketAddr>().unwrap(),
    ));
    assert_eq!(
        fixture
            .router
            .clone()
            .oneshot(cross_site)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let session_csrf = client.csrf.clone();
    for invalid_csrf in [None, preauth_csrf] {
        client.csrf = invalid_csrf;
        assert_eq!(
            client
                .call(&fixture, Method::POST, "/api/admin/logout", Some(json!({})))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    client.csrf = session_csrf;
    assert_eq!(
        client
            .call(
                &fixture,
                Method::POST,
                "/api/admin/logout",
                Some(json!({"admin":true}))
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .call(&fixture, Method::GET, "/api/admin/session", None)
            .await
            .0,
        StatusCode::OK
    );
    let oversized = hide(Uuid::new_v4(), vec![Uuid::new_v4()], &"x".repeat(64 * 1024));
    let (status, result, _) = client
        .call(
            &fixture,
            Method::POST,
            "/api/admin/strokes/hide",
            Some(oversized),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{result}");
    assert_eq!(result["error"]["code"], "payload_too_large");
    let mut unknown_field = hide(Uuid::new_v4(), vec![Uuid::new_v4()], "reason");
    unknown_field["admin"] = json!(true);
    assert_eq!(
        client
            .call(
                &fixture,
                Method::POST,
                "/api/admin/strokes/hide",
                Some(unknown_field)
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fixture.stop().await;
}

#[tokio::test]
async fn disabled_admin_entry_can_explain_configuration_without_granting_api_access() {
    let directory = tempfile::tempdir().unwrap();
    tokio::fs::create_dir(directory.path().join("admin"))
        .await
        .unwrap();
    tokio::fs::write(
        directory.path().join("admin/index.html"),
        "<!doctype html><title>Administration</title>",
    )
    .await
    .unwrap();
    let routes = admin::router(None, Some(directory.path().to_owned()));
    let response = routes
        .clone()
        .oneshot(Request::get("/admin/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    for path in [
        "/api/admin/session",
        "/api/admin/login/options",
        "/api/admin/enrollment/start",
        "/api/admin/strokes/hide",
    ] {
        let response = routes
            .clone()
            .oneshot(Request::post(path).body(Body::from("{}")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["error"]["code"],
            "admin_unavailable"
        );
    }
}

#[tokio::test]
async fn disabled_admin_routes_return_safe_uncached_errors() {
    let response = admin::router(None, None)
        .oneshot(
            Request::get("/api/admin/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["error"]["code"],
        "admin_unavailable"
    );
}
