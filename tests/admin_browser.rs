//! Opt-in real frontend + backend + PostgreSQL + native Chromium WebAuthn integration.
//! Only an isolated sqlx test database is provisioned; no production credentials or bypasses.

#![cfg(feature = "browser-integration")]

use endless_drawing_server::{
    admin::{self, AdminConfig, AdminService, provision},
    app,
    config::RealtimeSettings,
    domain::StrokeLimits,
    realtime::Realtime,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{net::SocketAddr, path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::task::AbortOnDropHandle;
use zeroize::Zeroizing;

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires FRONTEND_DIR with built dist, Node/Playwright Chromium, OpenSSL and PostgreSQL"]
async fn real_https_frontend_enrollment_login_hide_and_logout(pool: PgPool) {
    let frontend = PathBuf::from(std::env::var_os("FRONTEND_DIR").expect("set FRONTEND_DIR"));
    let directory = frontend.join("dist");
    assert!(
        directory.join("admin/index.html").is_file(),
        "build the complete frontend first"
    );
    let mut browser = tokio::process::Command::new("node")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/browser/admin.mjs"
        ))
        .arg(&frontend)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("start Node/Playwright");
    let mut output = BufReader::new(browser.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(20), output.read_line(&mut ready))
        .await
        .unwrap()
        .unwrap();
    let ready: Value = serde_json::from_str(&ready).expect("browser proxy readiness");
    let origin = ready["origin"].as_str().unwrap();
    let settings = RealtimeSettings {
        allowed_origins: vec![origin.into()],
        ..Default::default()
    };
    let (realtime, worker) =
        Realtime::start(pool.clone(), StrokeLimits::default(), settings).unwrap();
    let mut worker = AbortOnDropHandle::new(worker);
    let service = AdminService::new(
        pool.clone(),
        &realtime,
        AdminConfig::new(origin, "localhost".into())
            .unwrap()
            .with_totp_key([11; 32]),
    )
    .await
    .unwrap();
    let routes = app::router(
        pool.clone(),
        Duration::from_secs(2),
        Some(directory.clone()),
    )
    .merge(realtime.router())
    .merge(admin::router(Some(service), Some(directory)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = realtime.shutdown_token();
    let mut server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(
            listener,
            routes.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .unwrap();
    }));
    let password = Zeroizing::new("ephemeral browser-test manager password 57!".to_owned());
    let grant = provision::create(&pool, "browser-moderator", password.clone())
        .await
        .unwrap();
    let config = Zeroizing::new(
        serde_json::to_vec(&json!({
            "backend": format!("http://{address}"), "login": "browser-moderator",
            "password": *password, "enrollment_token": grant.secret(),
        }))
        .unwrap(),
    );
    let mut input = browser.stdin.take().unwrap();
    input.write_all(&config).await.unwrap();
    input.write_all(b"\n").await.unwrap();
    drop(input);
    let status = tokio::time::timeout(Duration::from_secs(150), browser.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        status.success(),
        "real browser integration failed (see sanitized step above)"
    );
    let (credentials, operations, hidden, active_sessions): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM admin_credentials WHERE revoked_at IS NULL),
                (SELECT count(*) FROM moderation_operations),
                (SELECT count(*) FROM strokes WHERE deleted_at IS NOT NULL),
                (SELECT count(*) FROM admin_sessions WHERE revoked_at IS NULL)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (credentials, operations, hidden, active_sessions),
        (2, 1, 1, 0)
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_security_audit WHERE event = 'strokes_hidden'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);
    realtime.shutdown_token().cancel();
    tokio::time::timeout(Duration::from_secs(5), &mut server)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut worker)
        .await
        .unwrap()
        .unwrap();
}
