const SAFETY: &str = "endless-drawing.msgpack.v5";

async fn safety_connect(server: &Server, seed: u8) -> Socket {
    let mut request = server.url.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    request.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("endless-drawing.msgpack.v5, endless-drawing.msgpack.v4, endless-drawing.json.v1"));
    let (mut socket, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()["Sec-WebSocket-Protocol"], SAFETY);
    binary_send(
        &mut socket,
        json!({"type":"hello","session_secret":URL_SAFE_NO_PAD.encode([seed;32])}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["protocol_version"], 5);
    socket
}

async fn safety_create(socket: &mut Socket, request: &str, id: Uuid) -> Value {
    binary_send(socket, json!({"type":"stroke.create","request_id":request,"stroke":stroke(id)})).await;
    until(socket, "ack").await
}

async fn safety_report(socket: &mut Socket, request: &str, ids: &[Uuid]) -> Value {
    binary_send(socket, json!({"type":"stroke.report","request_id":request,"stroke_ids":ids,"reason":"hate","block_author":true})).await;
    loop {
        let message = receive(socket).await;
        if message["type"] == "ack" || message["type"] == "error" {
            return message;
        }
    }
}

async fn safety_snapshot(socket: &mut Socket, id: &str) -> Vec<Value> {
    priority_full(socket, json!({"type":"viewport.subscribe","subscription_id":id,
      "bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100},
      "visible_bounds":{"min_x":-10,"min_y":-10,"max_x":10,"max_y":10}})).await
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn v5_author_tags_are_per_viewer_and_v4_has_none(pool: PgPool) {
    let mut server = Server::start(pool, RealtimeSettings::default()).await;
    let mut author = safety_connect(&server, 11).await;
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    let ack = safety_create(&mut author, "a", first).await;
    let own_tag = ack["stroke"]["author"].as_str().unwrap().to_owned();
    assert_eq!(own_tag.len(), 22);
    safety_create(&mut author, "b", second).await;

    let mut viewer = safety_connect(&server, 12).await;
    let seen = safety_snapshot(&mut viewer, "v").await;
    let tags: Vec<&str> = seen.iter().map(|s| s["author"].as_str().unwrap()).collect();
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0], tags[1], "one author, one tag for this viewer");
    assert_ne!(tags[0], own_tag, "tags differ between viewers");
    let mut other = safety_connect(&server, 13).await;
    let other_seen = safety_snapshot(&mut other, "o").await;
    assert_ne!(other_seen[0]["author"], seen[0]["author"]);

    let mut old = priority_connect(&server, 14).await;
    let old_seen = priority_full(&mut old, priority_subscription("p", false)).await;
    assert!(old_seen.iter().all(|s| s.get("author").is_none()), "v4 has no author field");
    binary_send(&mut old, json!({"type":"stroke.report","request_id":"r","stroke_ids":[first],"reason":"spam","block_author":false})).await;
    assert_eq!(receive(&mut old).await["code"], "invalid_message");
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn v5_reports_validate_hold_at_threshold_and_rate_limit(pool: PgPool) {
    let settings = RealtimeSettings { reports_per_hour: 3, report_hold_threshold: 2, ..Default::default() };
    let mut server = Server::start(pool.clone(), settings).await;
    let mut author = safety_connect(&server, 21).await;
    let target = Uuid::new_v4();
    safety_create(&mut author, "t", target).await;

    let mut first = safety_connect(&server, 22).await;
    safety_snapshot(&mut first, "s1").await;
    // Own, unknown and duplicate ids are rejected without storing anything.
    assert_eq!(safety_report(&mut author, "own", &[target]).await["code"], "validation_failed");
    assert_eq!(safety_report(&mut first, "missing", &[Uuid::new_v4()]).await["code"], "not_found");
    assert_eq!(safety_report(&mut first, "dup", &[target, target]).await["code"], "validation_failed");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM stroke_reports").fetch_one(&pool).await.unwrap();
    assert_eq!(stored, 0);

    let ack = safety_report(&mut first, "r1", &[target]).await;
    assert_eq!(ack["operation"], "stroke.report");
    // A retried request returns the same report; the same reporter again adds no weight.
    assert_eq!(safety_report(&mut first, "r1", &[target]).await["report_id"], ack["report_id"]);
    safety_report(&mut first, "r2", &[target]).await;
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM stroke_holds").fetch_one(&pool).await.unwrap();
    assert_eq!(held, 0, "one distinct reporter is below the threshold");
    // Hourly limit (3) counts stored reports, persisted across the connection.
    safety_report(&mut first, "r3", &[target]).await;
    assert_eq!(safety_report(&mut first, "r4", &[target]).await["code"], "rate_limited");

    // A second distinct reporter holds it: public delete for subscribers, absent from snapshots.
    let mut second = safety_connect(&server, 23).await;
    safety_snapshot(&mut second, "s2").await;
    assert_eq!(safety_report(&mut second, "x", &[target]).await["operation"], "stroke.report");
    let deleted = until(&mut first, "stroke.deleted").await;
    assert_eq!(deleted["stroke_id"], json!(target));
    assert_eq!(until(&mut second, "stroke.deleted").await["stroke_id"], json!(target));
    assert!(safety_snapshot(&mut second, "s3").await.is_empty());
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM stroke_holds WHERE released_at IS NULL").fetch_one(&pool).await.unwrap();
    assert_eq!(held, 1);
    // Held strokes can no longer be reported.
    let mut third = safety_connect(&server, 24).await;
    assert_eq!(safety_report(&mut third, "late", &[target]).await["code"], "not_found");
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn v5_banned_sessions_cannot_draw_or_report(pool: PgPool) {
    let mut server = Server::start(pool.clone(), RealtimeSettings::default()).await;
    let mut author = safety_connect(&server, 31).await;
    let mut other = safety_connect(&server, 32).await;
    let theirs = Uuid::new_v4();
    safety_create(&mut other, "o", theirs).await;
    let digest = Sha256::digest([31u8; 32]).to_vec();
    sqlx::query("INSERT INTO banned_owners (owner_digest) VALUES ($1)").bind(&digest).execute(&pool).await.unwrap();
    binary_send(&mut author, json!({"type":"stroke.create","request_id":"c","stroke":stroke(Uuid::new_v4())})).await;
    assert_eq!(receive(&mut author).await["code"], "banned");
    assert_eq!(safety_report(&mut author, "r", &[theirs]).await["code"], "banned");
    server.stop().await;
}
