const MANIFEST: &str = "endless-drawing.msgpack.v3";

async fn manifest_connect(server: &Server, seed: u8) -> Socket {
    let mut request = server.url.clone().into_client_request().unwrap();
    request.headers_mut().insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    request.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("endless-drawing.msgpack.v3, endless-drawing.msgpack.v2, endless-drawing.json.v1"));
    let (mut socket, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()["Sec-WebSocket-Protocol"], MANIFEST);
    binary_send(&mut socket, json!({"type":"hello","session_secret":URL_SAFE_NO_PAD.encode([seed;32])})).await;
    assert_eq!(receive(&mut socket).await["protocol_version"], 3);
    socket
}

async fn manifest_begin(socket: &mut Socket, id: &str) -> (Value, Vec<Value>) {
    binary_send(socket, json!({"type":"viewport.subscribe.cached","subscription_id":id,"bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100}})).await;
    assert_eq!(until(socket, "viewport.snapshot.begin").await["subscription_id"], id);
    let begin = receive(socket).await;
    assert_eq!(begin["type"], "viewport.manifest.begin");
    let mut ids = Vec::new();
    loop {
        let message = receive(socket).await;
        assert_eq!(message["subscription_id"], id);
        assert_eq!(message["snapshot_token"], begin["snapshot_token"]);
        if message["type"] == "viewport.manifest.end" { break; }
        assert_eq!(message["type"], "viewport.manifest.chunk");
        assert!(message["stroke_ids"].as_array().unwrap().len() <= 1000);
        ids.extend(message["stroke_ids"].as_array().unwrap().iter().cloned());
    }
    assert_eq!(begin["stroke_count"].as_u64().unwrap(), ids.len() as u64);
    (begin, ids)
}

async fn manifest_fetch(socket: &mut Socket, begin: &Value, ids: Vec<Value>) -> Vec<Value> {
    binary_send(socket, json!({"type":"viewport.fetch","subscription_id":begin["subscription_id"],
        "snapshot_token":begin["snapshot_token"],"batch":0,"stroke_ids":ids,"final":true})).await;
    let mut strokes = Vec::new();
    loop {
        let message = receive(socket).await;
        assert_eq!(message["subscription_id"], begin["subscription_id"]);
        if message["type"] == "viewport.snapshot.end" { break; }
        assert_eq!(message["type"], "viewport.snapshot.chunk");
        strokes.extend(message["strokes"].as_array().unwrap().iter().cloned());
    }
    strokes
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn manifest_warm_area_sends_no_geometry_and_partial_cache_fetches_only_missing(pool: PgPool) {
    let mut server = Server::start(pool, RealtimeSettings::default()).await;
    let mut author = server.connect(70).await;
    let a = Uuid::new_v4(); let b = Uuid::new_v4();
    create(&mut author, "a", stroke(a)).await; create(&mut author, "b", stroke(b)).await;
    let mut visitor = manifest_connect(&server, 71).await;
    assert_eq!(binary_subscribe(&mut visitor, "cold").await.len(), 2);
    let (begin, ids) = manifest_begin(&mut visitor, "warm").await;
    assert_eq!(ids, vec![json!(a),json!(b)]);
    assert_eq!(begin["point_count"], 4);
    assert!(manifest_fetch(&mut visitor, &begin, vec![]).await.is_empty());
    let (begin, _) = manifest_begin(&mut visitor, "partial").await;
    let geometry = manifest_fetch(&mut visitor, &begin, vec![json!(b)]).await;
    assert_eq!(geometry.len(), 1); assert_eq!(geometry[0]["id"], b.to_string());
    assert_eq!(geometry[0]["own"], false);
    send(&mut author, json!({"type":"stroke.delete","request_id":"erase","stroke_id":a})).await;
    until(&mut author, "ack").await;
    let (begin, ids) = manifest_begin(&mut visitor, "after-delete").await;
    assert_eq!(ids, vec![json!(b)]);
    assert!(manifest_fetch(&mut visitor, &begin, vec![]).await.is_empty());
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn manifest_releases_database_while_waiting_and_delivers_post_snapshot_delete(pool: PgPool) {
    let single = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect_with(pool.connect_options().as_ref().clone()).await.unwrap();
    let mut server = Server::start(single.clone(), RealtimeSettings::default()).await;
    let mut author = server.connect(72).await;
    let a = Uuid::new_v4(); create(&mut author, "a", stroke(a)).await;
    let mut visitor = manifest_connect(&server, 73).await;
    let (begin, ids) = manifest_begin(&mut visitor, "frozen").await;
    let connection = timeout(Duration::from_secs(1), single.acquire()).await.expect("manifest must release pool before client fetch").unwrap();
    drop(connection);
    send(&mut author, json!({"type":"stroke.delete","request_id":"erase","stroke_id":a})).await;
    until(&mut author, "ack").await;
    let geometry = manifest_fetch(&mut visitor, &begin, ids).await;
    assert_eq!(geometry[0]["id"], a.to_string());
    assert_eq!(until(&mut visitor, "stroke.deleted").await["stroke_id"], a.to_string());
    assert!(binary_subscribe(&mut visitor, "fresh").await.is_empty());
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn manifest_rejects_scope_token_batch_and_duplicate_requests_and_ignores_old_subscription(pool: PgPool) {
    let mut server = Server::start(pool, RealtimeSettings::default()).await;
    let mut author = server.connect(74).await;
    let a = Uuid::new_v4(); create(&mut author, "a", stroke(a)).await;
    for invalid in 0..4 {
        let mut visitor = manifest_connect(&server, 75 + invalid).await;
        let (begin, ids) = manifest_begin(&mut visitor, "invalid").await;
        let mut request = json!({"type":"viewport.fetch","subscription_id":"invalid","snapshot_token":begin["snapshot_token"],"batch":0,"stroke_ids":ids,"final":true});
        match invalid {
            0 => request["snapshot_token"] = json!(Uuid::new_v4()),
            1 => request["stroke_ids"] = json!([Uuid::new_v4()]),
            2 => request["batch"] = json!(1),
            _ => request["stroke_ids"] = json!([a,a]),
        }
        binary_send(&mut visitor, request).await;
        assert_eq!(receive(&mut visitor).await["code"], "resync_required");
    }
    let mut visitor = manifest_connect(&server, 79).await;
    let (old, _) = manifest_begin(&mut visitor, "old").await;
    let (new, _) = manifest_begin(&mut visitor, "new").await;
    binary_send(&mut visitor, json!({"type":"viewport.fetch","subscription_id":"old","snapshot_token":old["snapshot_token"],"batch":0,"stroke_ids":[],"final":true})).await;
    assert!(manifest_fetch(&mut visitor, &new, vec![]).await.is_empty());
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn manifest_physical_row_loss_and_event_overflow_require_resync(pool: PgPool) {
    let settings = RealtimeSettings { event_buffer_capacity: 2, creates_per_second: 100, ..Default::default() };
    let mut server = Server::start(pool.clone(), settings).await;
    let mut author = server.connect(80).await;
    let a = Uuid::new_v4(); create(&mut author, "a", stroke(a)).await;
    let mut visitor = manifest_connect(&server, 81).await;
    let (begin, ids) = manifest_begin(&mut visitor, "purged").await;
    sqlx::query("DELETE FROM strokes WHERE id=$1").bind(a).execute(&pool).await.unwrap();
    binary_send(&mut visitor, json!({"type":"viewport.fetch","subscription_id":"purged","snapshot_token":begin["snapshot_token"],"batch":0,"stroke_ids":ids,"final":true})).await;
    assert_eq!(receive(&mut visitor).await["code"], "resync_required");
    let (begin, _) = manifest_begin(&mut visitor, "overflow").await;
    for i in 0..4 { create(&mut author, &format!("create-{i}"), stroke(Uuid::new_v4())).await; }
    assert!(manifest_fetch(&mut visitor, &begin, vec![]).await.is_empty());
    assert_eq!(receive(&mut visitor).await["code"], "resync_required");
    assert_eq!(binary_subscribe(&mut visitor, "recovered").await.len(), 4);
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn manifest_timeout_busy_admission_and_cancellation_release_resources(pool: PgPool) {
    let mut server = Server::start(pool, RealtimeSettings::default()).await;
    let mut sockets = Vec::new();
    for seed in 90..106 {
        let mut socket = manifest_connect(&server, seed).await;
        manifest_begin(&mut socket, "held").await;
        sockets.push(socket);
    }
    let mut waiting = manifest_connect(&server, 106).await;
    binary_send(&mut waiting, json!({"type":"viewport.subscribe.cached","subscription_id":"busy","bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100}})).await;
    let fallback = receive(&mut waiting).await;
    assert_eq!(fallback["type"], "viewport.snapshot.fallback"); assert_eq!(fallback["reason"], "busy");
    let mut cancelled = sockets.pop().unwrap(); cancelled.close(None).await.unwrap();
    timeout(Duration::from_secs(2), cancelled.next()).await.unwrap();
    let (begin, _) = manifest_begin(&mut waiting, "released").await;
    assert!(manifest_fetch(&mut waiting, &begin, vec![]).await.is_empty());
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn manifest_slow_client_times_out_without_holding_pool(pool: PgPool) {
    let settings = RealtimeSettings { snapshot_timeout: Duration::from_millis(200), ..Default::default() };
    let mut server = Server::start(pool, settings).await;
    let mut visitor = manifest_connect(&server, 108).await;
    manifest_begin(&mut visitor, "slow").await;
    assert_eq!(receive(&mut visitor).await["code"], "resync_required");
    assert!(binary_subscribe(&mut visitor, "recovered").await.is_empty());
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn manifest_can_be_disabled_while_messagepack_v2_remains_available(pool: PgPool) {
    let settings = RealtimeSettings { manifest_enabled: false, ..Default::default() };
    let mut server = Server::start(pool, settings).await;
    let mut request = server.url.clone().into_client_request().unwrap();
    request.headers_mut().insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    request.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("endless-drawing.msgpack.v3, endless-drawing.msgpack.v2, endless-drawing.json.v1"));
    let (_, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()["Sec-WebSocket-Protocol"], BINARY);
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn manifest_rejects_dense_membership_without_claiming_a_partial_snapshot(pool: PgPool) {
    let points = json!(vec![[0, 0]; 2000]);
    sqlx::query("INSERT INTO strokes(id,owner_digest,points,brush,width,color,opacity,bounds)
        SELECT gen_random_uuid(),$1,$2,'round',6,'#202020',1,box(point(-4,-4),point(4,4)) FROM generate_series(1,501)")
        .bind(vec![0_u8;32]).bind(points).execute(&pool).await.unwrap();
    let mut server = Server::start(pool, RealtimeSettings::default()).await;
    let mut socket = manifest_connect(&server, 109).await;
    binary_send(&mut socket, json!({"type":"viewport.subscribe.cached","subscription_id":"dense","bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100}})).await;
    assert_eq!(receive(&mut socket).await["type"], "viewport.snapshot.begin");
    let fallback = receive(&mut socket).await;
    assert_eq!(fallback["type"], "viewport.snapshot.fallback");
    assert_eq!(fallback["reason"], "scene_limit");
    binary_send(&mut socket, json!({"type":"viewport.subscribe","subscription_id":"far","bounds":{"min_x":1000,"min_y":1000,"max_x":1100,"max_y":1100}})).await;
    assert_eq!(receive(&mut socket).await["type"], "viewport.snapshot.begin");
    assert_eq!(receive(&mut socket).await["type"], "viewport.snapshot.end");
    server.stop().await;
}
