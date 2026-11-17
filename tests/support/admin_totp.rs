//! Ephemeral fixtures only. Never operator secrets, production tokens or a bypass.
use super::*;
use hmac::{Hmac, Mac};
use sha1::Sha1;

fn totp(secret: &str, step: i64) -> String {
    let bytes = data_encoding::BASE32_NOPAD
        .decode(secret.as_bytes())
        .unwrap();
    let mut mac = Hmac::<Sha1>::new_from_slice(&bytes).unwrap();
    mac.update(&step.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = usize::from(hash[19] & 15);
    let value = u32::from_be_bytes(hash[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
    format!("{:06}", value % 1_000_000)
}
async fn step(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp())/30)::bigint")
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn enrolled_one(fixture: &mut Fixture) -> Client {
    let grant = provision::create(&fixture.pool, LOGIN, Zeroizing::new(PASSWORD.into()))
        .await
        .unwrap();
    let mut client = Client::default();
    client.options(fixture).await;
    let (status,challenge,_)=client.call(fixture,Method::POST,"/api/admin/enrollment/start",Some(json!({"login":LOGIN,"password":PASSWORD,"enrollment_token":grant.secret(),"label":"Only primary"}))).await;
    assert_eq!(status, StatusCode::OK);
    let credential = fixture
        .primary
        .do_registration(
            Url::parse(ORIGIN).unwrap(),
            serde_json::from_value(challenge["webauthn"].clone()).unwrap(),
        )
        .unwrap();
    let (status, result, _) = client
        .call(
            fixture,
            Method::POST,
            "/api/admin/enrollment/finish",
            Some(json!({"credential":credential})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["complete"], true);
    assert_eq!(result["required_credentials"], 1);
    client
}

async fn setup(fixture: &mut Fixture, client: &mut Client) -> String {
    fixture.login(client, false).await;
    let (status, reply, _) = client
        .call(
            fixture,
            Method::POST,
            "/api/admin/totp/setup/start",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let secret = reply["secret"].as_str().unwrap().to_owned();
    assert!(
        reply["otpauth_uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/")
    );
    let code = totp(&secret, step(&fixture.pool).await);
    assert_eq!(
        client
            .call(
                fixture,
                Method::POST,
                "/api/admin/totp/setup/confirm",
                Some(json!({"code":code}))
            )
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    secret
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn totp_requires_password_and_cannot_manage_factors_or_replay(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut uv = enrolled_one(&mut fixture).await;
    let secret = setup(&mut fixture, &mut uv).await;
    let mut backup = Client::default();
    backup.options(&fixture).await;
    let code = totp(&secret, step(&pool).await + 1);
    let mut payload =
        json!({"login":LOGIN,"password":"wrong manager password long enough!","code":code});
    assert_eq!(
        backup
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login/totp",
                Some(payload.clone())
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    payload["password"] = json!(PASSWORD);
    let (status, session, _) = backup
        .call(
            &fixture,
            Method::POST,
            "/api/admin/login/totp",
            Some(payload.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["auth_method"], "totp");
    backup.csrf = Some(session["csrf_token"].as_str().unwrap().into());
    for path in ["/api/admin/totp/setup/start", "/api/admin/totp/disable"] {
        assert_eq!(
            backup
                .call(&fixture, Method::POST, path, Some(json!({})))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let mut replay = Client::default();
    replay.options(&fixture).await;
    assert_eq!(
        replay
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login/totp",
                Some(payload)
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // Disabling from fresh UV revokes the fallback session atomically.
    assert_eq!(
        uv.call(
            &fixture,
            Method::POST,
            "/api/admin/totp/disable",
            Some(json!({}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        backup
            .call(&fixture, Method::GET, "/api/admin/session", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    fixture.stop().await;
}
#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn totp_concurrent_same_step_creates_only_one_session(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut uv = enrolled_one(&mut fixture).await;
    let secret = setup(&mut fixture, &mut uv).await;
    let mut first = Client::default();
    first.options(&fixture).await;
    let mut second = Client::default();
    second.options(&fixture).await;
    let payload =
        json!({"login":LOGIN,"password":PASSWORD,"code":totp(&secret,step(&pool).await+1)});
    let (a, b) = tokio::join!(
        first.call(
            &fixture,
            Method::POST,
            "/api/admin/login/totp",
            Some(payload.clone())
        ),
        second.call(
            &fixture,
            Method::POST,
            "/api/admin/login/totp",
            Some(payload)
        )
    );
    let statuses = [a.0, b.0];
    assert_eq!(statuses.iter().filter(|&&s| s == StatusCode::OK).count(), 1);
    assert_eq!(
        statuses
            .iter()
            .filter(|&&s| s == StatusCode::UNAUTHORIZED)
            .count(),
        1
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admin_sessions WHERE auth_method='totp'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let used: i64 = sqlx::query_scalar("SELECT last_used_step FROM admin_totp")
        .fetch_one(&pool)
        .await
        .unwrap();
    // New service/rate counters do not reset the durable replay barrier.
    let service = AdminService::new(
        pool.clone(),
        &fixture.realtime,
        AdminConfig::new(ORIGIN, "example.test".into())
            .unwrap()
            .with_totp_key([7; 32]),
    )
    .await
    .unwrap();
    fixture.router = admin::router(Some(service), None);
    let mut after_restart = Client::default();
    after_restart.options(&fixture).await;
    assert_eq!(
        after_restart
            .call(
                &fixture,
                Method::POST,
                "/api/admin/login/totp",
                Some(json!({"login":LOGIN,"password":PASSWORD,"code":totp(&secret,used)}))
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    fixture.stop().await;
}
#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn totp_pending_is_encrypted_expiring_and_bound_to_fresh_uv_session(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut uv = fixture.enrolled().await;
    fixture.login(&mut uv, false).await;
    let (status, start, _) = uv
        .call(
            &fixture,
            Method::POST,
            "/api/admin/totp/setup/start",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let raw = data_encoding::BASE32_NOPAD
        .decode(start["secret"].as_str().unwrap().as_bytes())
        .unwrap();
    let encrypted: Vec<u8> = sqlx::query_scalar("SELECT pending_secret FROM admin_totp")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(encrypted.len(), 49);
    assert!(!encrypted.windows(raw.len()).any(|part| part == raw));
    let mut another = Client::default();
    fixture.login(&mut another, true).await;
    let payload = json!({"code":totp(start["secret"].as_str().unwrap(),step(&pool).await)});
    assert_eq!(
        another
            .call(
                &fixture,
                Method::POST,
                "/api/admin/totp/setup/confirm",
                Some(payload.clone())
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE admin_totp SET pending_expires_at=TIMESTAMPTZ '2000-01-01Z'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        uv.call(
            &fixture,
            Method::POST,
            "/api/admin/totp/setup/confirm",
            Some(payload)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE admin_sessions SET created_at=TIMESTAMPTZ '2000-01-01Z'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        uv.call(
            &fixture,
            Method::POST,
            "/api/admin/totp/setup/start",
            Some(json!({}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    fixture.stop().await;
}
#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn totp_missing_wrong_key_and_recovery_fail_closed(pool: PgPool) {
    let mut fixture = Fixture::new(pool.clone()).await;
    let mut uv = fixture.enrolled().await;
    setup(&mut fixture, &mut uv).await;
    for key in [None, Some([9; 32])] {
        let mut config = AdminConfig::new(ORIGIN, "example.test".into()).unwrap();
        if let Some(key) = key {
            config = config.with_totp_key(key);
        }
        assert!(
            AdminService::new(pool.clone(), &fixture.realtime, config)
                .await
                .is_err()
        );
    }
    provision::recover(&pool, LOGIN, Zeroizing::new(PASSWORD.into()))
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_totp")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        uv.call(&fixture, Method::GET, "/api/admin/session", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    fixture.stop().await;
}
