// Shared public-WS profile tests. Not an admin authentication transport.
const BINARY: &str = "endless-drawing.msgpack.v2";
const JSON: &str = "endless-drawing.json.v1";

async fn binary_connect(server: &Server, seed: u8) -> Socket {
    let mut request = server.url.clone().into_client_request().unwrap();
    request.headers_mut().insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    request.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("endless-drawing.msgpack.v2, endless-drawing.json.v1"));
    let (mut socket, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()["Sec-WebSocket-Protocol"], BINARY);
    binary_send(&mut socket, json!({"type":"hello","session_secret":URL_SAFE_NO_PAD.encode([seed;32])})).await;
    assert_eq!(receive(&mut socket).await["protocol_version"], 2);
    socket
}

async fn binary_send(socket: &mut Socket, value: Value) {
    socket.send(Message::Binary(rmp_serde::to_vec_named(&value).unwrap().into())).await.unwrap();
}

async fn binary_subscribe(socket: &mut Socket, id: &str) -> Vec<Value> {
    binary_send(socket, json!({"type":"viewport.subscribe","subscription_id":id,"bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100}})).await;
    assert_eq!(until(socket, "viewport.snapshot.begin").await["subscription_id"], id);
    let mut result = Vec::new();
    loop {
        let value = receive(socket).await;
        assert_eq!(value["subscription_id"], id);
        match value["type"].as_str().unwrap() {
            "viewport.snapshot.chunk" => {
                let strokes = value["strokes"].as_array().unwrap();
                assert!(strokes.len() <= 1000);
                assert!(strokes.iter().map(|s| s["points"].as_array().unwrap().len()).sum::<usize>() <= 8192);
                assert!(rmp_serde::to_vec_named(&value).unwrap().len() <= 64 * 1024);
                result.extend(strokes.iter().cloned());
            }
            "viewport.snapshot.end" => return result,
            other => panic!("unexpected snapshot type: {other}"),
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn binary_and_legacy_clients_share_strokes_retry_ownership_and_deletes(pool: PgPool) {
    let mut server = Server::start(pool, RealtimeSettings::default()).await;
    let mut author = binary_connect(&server, 41).await;
    let mut visitor = server.connect(42).await;
    assert!(binary_subscribe(&mut author, "author").await.is_empty());
    assert!(subscribe(&mut visitor, "visitor").await.is_empty());
    let id = Uuid::new_v4();
    let payload = json!({"type":"stroke.create","request_id":"binary-create","stroke":stroke(id)});
    binary_send(&mut author, payload.clone()).await;
    let ack = until(&mut author, "ack").await;
    assert_eq!(ack["stroke"]["id"], id.to_string());
    assert_eq!(ack["stroke"]["own"], true);
    assert_eq!(until(&mut visitor, "stroke.created").await["stroke"]["own"], false);
    binary_send(&mut author, payload).await;
    assert_eq!(until(&mut author, "ack").await["stroke"], ack["stroke"]);
    send(&mut visitor, json!({"type":"stroke.delete","request_id":"foreign","stroke_id":id})).await;
    assert_eq!(until(&mut visitor, "error").await["code"], "forbidden");
    author.close(None).await.unwrap();
    let mut reloaded = binary_connect(&server, 41).await;
    let snapshot = binary_subscribe(&mut reloaded, "reload").await;
    assert_eq!(snapshot.len(), 1); assert_eq!(snapshot[0]["own"], true);
    binary_send(&mut reloaded, json!({"type":"stroke.delete","request_id":"erase","stroke_id":id})).await;
    assert_eq!(until(&mut reloaded, "ack").await["stroke_id"], id.to_string());
    assert_eq!(until(&mut visitor, "stroke.deleted").await["stroke_id"], id.to_string());
    assert!(binary_subscribe(&mut reloaded, "after-delete").await.is_empty());
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn binary_snapshot_is_chunked_in_encoded_bytes_with_precise_geometry(pool: PgPool) {
    let settings = RealtimeSettings { creates_per_second: 100, ..Default::default() };
    let mut server = Server::start(pool, settings).await;
    let mut author = binary_connect(&server, 43).await;
    for i in 0..10 {
        let mut input = stroke(Uuid::new_v4());
        input["points"] = Value::Array((0..2000).map(|n| json!([f64::from(n) * 0.03125, f64::from(i) + 0.0625])).collect());
        input["brush"] = json!(["round", "square", "chisel", "spray"][i as usize % 4]);
        binary_send(&mut author, json!({"type":"stroke.create","request_id":format!("create-{i}"),"stroke":input})).await;
        let ack = until(&mut author, "ack").await;
        assert_eq!(ack["stroke"]["points"][0][1], f64::from(i) + 0.0625);
    }
    let all = binary_subscribe(&mut author, "dense").await;
    assert_eq!(all.len(), 10);
    assert!(all.windows(2).all(|s| s[0]["seq"].as_i64() < s[1]["seq"].as_i64()));
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn explicit_json_fallback_and_unsupported_protocols(pool: PgPool) {
    let settings = RealtimeSettings { msgpack_enabled: false, ..Default::default() };
    let mut server = Server::start(pool, settings).await;
    let mut request = server.url.clone().into_client_request().unwrap();
    request.headers_mut().insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    request.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("endless-drawing.msgpack.v2, endless-drawing.json.v1"));
    let (mut socket, response) = connect_async(request.clone()).await.unwrap();
    assert_eq!(response.headers()["Sec-WebSocket-Protocol"], JSON);
    send(&mut socket, json!({"type":"hello","session_secret":URL_SAFE_NO_PAD.encode([44;32])})).await;
    assert_eq!(receive(&mut socket).await["protocol_version"], 1);
    assert!(subscribe(&mut socket, "json").await.is_empty());
    request.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("endless-drawing.msgpack.v2"));
    let error = connect_async(request).await.unwrap_err();
    assert!(matches!(error, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == 400));
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn malformed_binary_and_wrong_frame_type_close_without_domain_mutation(pool: PgPool) {
    let mut server = Server::start(pool.clone(), RealtimeSettings::default()).await;
    for (frame, code) in [
        (Message::Binary(vec![0; 128 * 1024 + 1].into()), 1009),
        (Message::Binary(vec![0x82, 0xa1, b'a', 1, 0xa1, b'a', 2].into()), 1007),
        (Message::Text("{}".into()), 1003),
        (Message::Binary(vec![0x81, 0xa1, b'a', 0xdd, 255, 255, 255, 255].into()), 1007),
    ] {
        let mut socket = binary_connect(&server, 45).await;
        socket.send(frame).await.unwrap();
        let next = timeout(Duration::from_secs(5), socket.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(next, Message::Close(Some(close)) if u16::from(close.code) == code));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM strokes").fetch_one(&pool).await.unwrap();
    assert_eq!(count, 0);
    server.stop().await;
}
