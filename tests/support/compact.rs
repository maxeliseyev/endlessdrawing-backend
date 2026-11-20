const COMPACT: &str = "endless-drawing.msgpack.v6";
const ALL_BINARY: &str = "endless-drawing.msgpack.v6, endless-drawing.msgpack.v5, endless-drawing.msgpack.v4, endless-drawing.msgpack.v3, endless-drawing.msgpack.v2, endless-drawing.json.v1";

async fn offer(server: &Server, protocols: &'static str, seed: u8) -> (Socket, String, Value) {
    let mut request = server.url.clone().into_client_request().unwrap();
    request.headers_mut().insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    request.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static(protocols));
    let (mut socket, response) = connect_async(request).await.unwrap();
    let selected = response.headers()["Sec-WebSocket-Protocol"].to_str().unwrap().to_owned();
    binary_send(&mut socket, json!({"type":"hello","session_secret":URL_SAFE_NO_PAD.encode([seed;32])})).await;
    let ack = receive(&mut socket).await;
    (socket, selected, ack)
}

async fn compact_connect(server: &Server, seed: u8) -> Socket {
    let (socket, selected, ack) = offer(server, ALL_BINARY, seed).await;
    assert_eq!(selected, COMPACT);
    assert_eq!(ack["protocol_version"], 6);
    socket
}

/// Integer path → world points, as a v6 client decodes it.
fn decode_path(path: &Value) -> Vec<[f64; 2]> {
    let values: Vec<i64> = path.as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
    assert_eq!(values.len() % 2, 0);
    let (mut x, mut y) = (0, 0);
    values.chunks(2).map(|pair| { x += pair[0]; y += pair[1]; [x as f64 / 16.0, y as f64 / 16.0] }).collect()
}

const COMPACT_POINTS: [[f64; 2]; 3] = [[0.03, -2.5], [10.0, 5.0], [10.0451, 5.0234]];

fn compact_stroke(id: Uuid) -> Value {
    json!({"id":id,"points":COMPACT_POINTS,"brush":"round","width":6,"color":"#aabbcc","opacity":0.8})
}

fn assert_compact(stroke: &Value) {
    assert!(stroke.get("points").is_none(), "v6 strokes carry no points: {stroke}");
    assert_eq!(stroke["path"], json!([0, -40, 160, 120, 1, 0]));
    for (decoded, original) in decode_path(&stroke["path"]).iter().zip(COMPACT_POINTS) {
        assert!((decoded[0] - original[0]).abs() <= 1.0 / 32.0 && (decoded[1] - original[1]).abs() <= 1.0 / 32.0);
    }
    assert_eq!(stroke["author"].as_str().unwrap().len(), 22, "v6 keeps the v5 author tag");
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn v6_is_preferred_and_gated_while_v5_and_older_stay_selectable(pool: PgPool) {
    let mut server = Server::start(pool.clone(), RealtimeSettings::default()).await;
    compact_connect(&server, 41).await;
    let (_, selected, ack) = offer(&server, "endless-drawing.msgpack.v5, endless-drawing.msgpack.v4", 42).await;
    assert_eq!((selected.as_str(), ack["protocol_version"].as_u64()), (SAFETY, Some(5)));
    server.stop().await;
    let mut gated = Server::start(pool, RealtimeSettings { compact_enabled: false, ..Default::default() }).await;
    let (_, selected, ack) = offer(&gated, ALL_BINARY, 43).await;
    assert_eq!((selected.as_str(), ack["protocol_version"].as_u64()), (SAFETY, Some(5)));
    gated.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn v6_sends_paths_everywhere_while_v5_viewers_keep_exact_points(pool: PgPool) {
    let mut server = Server::start(pool, RealtimeSettings::default()).await;
    let mut watcher = compact_connect(&server, 51).await;
    assert!(priority_full(&mut watcher, priority_subscription("w", false)).await.is_empty());

    // The ack of a v6 create is a v6 stroke, uplink points stay floating point.
    let mut author = compact_connect(&server, 52).await;
    let id = Uuid::new_v4();
    binary_send(&mut author, json!({"type":"stroke.create","request_id":"c","stroke":compact_stroke(id)})).await;
    let ack = until(&mut author, "ack").await;
    assert_compact(&ack["stroke"]);
    assert_eq!(ack["stroke"]["own"], true);
    let live = until(&mut watcher, "stroke.created").await;
    assert_compact(&live["stroke"]);

    // Full snapshot, then a cached subscribe whose manifest counts points and whose fetch sends paths.
    let mut viewer = compact_connect(&server, 53).await;
    let full = priority_full(&mut viewer, priority_subscription("f", false)).await;
    assert_eq!(full.len(), 1);
    assert_compact(&full[0]);
    binary_send(&mut viewer, priority_subscription("c", true)).await;
    assert_eq!(until(&mut viewer, "viewport.snapshot.begin").await["subscription_id"], "c");
    let begin = receive(&mut viewer).await;
    assert_eq!(begin["type"], "viewport.manifest.begin");
    assert_eq!((begin["stroke_count"].as_u64(), begin["point_count"].as_u64()), (Some(1), Some(3)));
    let chunk = receive(&mut viewer).await;
    assert_eq!(chunk["stroke_ids"], json!([id]));
    assert_eq!(receive(&mut viewer).await["type"], "viewport.manifest.end");
    let fetched = manifest_fetch(&mut viewer, &begin, vec![json!(id)]).await;
    assert_compact(&fetched[0]);

    // The same stored stroke reaches a v5 viewer bit-exact.
    let mut old = safety_connect(&server, 54).await;
    let seen = safety_snapshot(&mut old, "o").await;
    assert!(seen[0].get("path").is_none());
    assert_eq!(seen[0]["points"], json!(COMPACT_POINTS));
    server.stop().await;
}
