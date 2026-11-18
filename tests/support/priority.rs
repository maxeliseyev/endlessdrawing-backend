const PRIORITY: &str = "endless-drawing.msgpack.v4";

async fn priority_connect(server: &Server, seed: u8) -> Socket {
    let mut request = server.url.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    request.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("endless-drawing.msgpack.v4, endless-drawing.msgpack.v3, endless-drawing.msgpack.v2, endless-drawing.json.v1"));
    let (mut socket, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()["Sec-WebSocket-Protocol"], PRIORITY);
    binary_send(
        &mut socket,
        json!({"type":"hello","session_secret":URL_SAFE_NO_PAD.encode([seed;32])}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["protocol_version"], 4);
    socket
}

fn priority_subscription(id: &str, cached: bool) -> Value {
    json!({"type":if cached {"viewport.subscribe.cached"} else {"viewport.subscribe"},"subscription_id":id,
      "bounds":{"min_x":-100,"min_y":-100,"max_x":100,"max_y":100},
      "visible_bounds":{"min_x":-10,"min_y":-10,"max_x":10,"max_y":10}})
}

async fn priority_full(socket: &mut Socket, payload: Value) -> Vec<Value> {
    binary_send(socket, payload).await;
    assert_eq!(receive(socket).await["type"], "viewport.snapshot.begin");
    let mut strokes = Vec::new();
    loop {
        let message = receive(socket).await;
        if message["type"] == "viewport.snapshot.end" {
            return strokes;
        }
        assert_eq!(message["type"], "viewport.snapshot.chunk");
        strokes.extend(message["strokes"].as_array().unwrap().iter().cloned());
    }
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn priority_full_and_missing_geometry_preserve_membership_and_seq_composition(pool: PgPool) {
    let settings = RealtimeSettings {
        snapshot_page_size: 1,
        subscribes_per_second: 20,
        ..Default::default()
    };
    let mut server = Server::start(pool, settings).await;
    let mut author = server.connect(101).await;
    let off = Uuid::new_v4();
    let visible = Uuid::new_v4();
    let spanning = Uuid::new_v4();
    let touching = Uuid::new_v4();
    let mut input = stroke(off);
    input["points"] = json!([[70, 70], [80, 80]]);
    create(&mut author, "off", input).await;
    create(&mut author, "visible", stroke(visible)).await;
    let mut input = stroke(spanning);
    input["points"] = json!([[-80, 0], [80, 0]]);
    create(&mut author, "spanning", input).await;
    let mut input = stroke(touching);
    input["points"] = json!([[14, 0]]); // width 6 + bbox padding reaches visible max_x=10 exactly.
    create(&mut author, "touching", input).await;
    let mut client = priority_connect(&server, 102).await;
    let all = priority_full(&mut client, priority_subscription("full", false)).await;
    assert_eq!(
        all.iter().map(|s| s["id"].clone()).collect::<Vec<_>>(),
        vec![json!(visible), json!(spanning), json!(touching), json!(off)]
    );
    assert!(all[0]["seq"].as_i64().unwrap() < all[1]["seq"].as_i64().unwrap());
    assert!(all[3]["seq"].as_i64().unwrap() < all[0]["seq"].as_i64().unwrap());
    binary_send(&mut client, priority_subscription("cached", true)).await;
    assert_eq!(
        receive(&mut client).await["type"],
        "viewport.snapshot.begin"
    );
    let begin = receive(&mut client).await;
    let mut ids = Vec::new();
    loop {
        let message = receive(&mut client).await;
        if message["type"] == "viewport.manifest.end" {
            break;
        }
        ids.extend(message["stroke_ids"].as_array().unwrap().iter().cloned());
    }
    assert_eq!(
        ids,
        vec![json!(visible), json!(spanning), json!(touching), json!(off)]
    );
    assert_eq!(begin["point_count"], 7);
    // A partially warm cache asks only the two missing members, in manifest priority order.
    let geometry = manifest_fetch(&mut client, &begin, vec![json!(visible), json!(off)]).await;
    assert_eq!(
        geometry.iter().map(|s| s["id"].clone()).collect::<Vec<_>>(),
        vec![json!(visible), json!(off)]
    );
    binary_send(&mut client, priority_subscription("warm", true)).await;
    assert_eq!(
        receive(&mut client).await["type"],
        "viewport.snapshot.begin"
    );
    let begin = receive(&mut client).await;
    loop {
        if receive(&mut client).await["type"] == "viewport.manifest.end" {
            break;
        }
    }
    assert!(manifest_fetch(&mut client, &begin, vec![]).await.is_empty());
    // A valid empty visible rectangle must not suppress prefetch members.
    let mut empty = priority_subscription("empty-visible", false);
    empty["visible_bounds"] = json!({"min_x":-95,"min_y":-95,"max_x":-90,"max_y":-90});
    let rest = priority_full(&mut client, empty).await;
    assert_eq!(rest.len(), 4);
    assert_eq!(rest[0]["id"], json!(off));
    // Legacy v3 still gets monotonically ordered full snapshots.
    let mut legacy = manifest_connect(&server, 103).await;
    let old = binary_subscribe(&mut legacy, "legacy").await;
    assert_eq!(old.len(), 4);
    assert_eq!(old[0]["id"], json!(off));
    server.stop().await;
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires PostgreSQL and DATABASE_URL"]
async fn priority_rejects_bad_hints_and_rolls_back_to_v3(pool: PgPool) {
    let settings = RealtimeSettings {
        subscribes_per_second: 30,
        ..Default::default()
    };
    let mut server = Server::start(pool.clone(), settings).await;
    let mut client = priority_connect(&server, 104).await;
    for (i, hint) in [
        json!({}),
        json!({"min_x":-101,"min_y":-1,"max_x":1,"max_y":1}),
        json!({"min_x":0,"min_y":0,"max_x":0,"max_y":1}),
        json!({"min_x":-1e10,"min_y":-1,"max_x":1,"max_y":1}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut payload = priority_subscription(&format!("bad-{i}"), false);
        payload["visible_bounds"] = hint;
        binary_send(&mut client, payload).await;
        assert_eq!(
            until(&mut client, "error").await["code"],
            if i == 0 {
                "invalid_message"
            } else {
                "validation_failed"
            }
        );
    }
    let mut absent = priority_subscription("missing", false);
    absent.as_object_mut().unwrap().remove("visible_bounds");
    binary_send(&mut client, absent).await;
    assert_eq!(until(&mut client, "error").await["code"], "invalid_message");
    let mut legacy = manifest_connect(&server, 105).await;
    binary_send(&mut legacy, priority_subscription("wrong-profile", false)).await;
    assert_eq!(until(&mut legacy, "error").await["code"], "invalid_message");
    server.stop().await;
    let mut server = Server::start(
        pool,
        RealtimeSettings {
            priority_enabled: false,
            ..Default::default()
        },
    )
    .await;
    let mut request = server.url.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static("http://localhost:5173"));
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(
            "endless-drawing.msgpack.v4, endless-drawing.msgpack.v3, endless-drawing.json.v1",
        ),
    );
    let (mut socket, response) = connect_async(request).await.unwrap();
    assert_eq!(response.headers()["Sec-WebSocket-Protocol"], MANIFEST);
    binary_send(
        &mut socket,
        json!({"type":"hello","session_secret":URL_SAFE_NO_PAD.encode([106;32])}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["protocol_version"], 3);
    server.stop().await;
}
