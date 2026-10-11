use super::*;
use std::net::TcpListener;
use xenomit_packet_client::model::{Collectable, CollectableKind, Entity, HeroState};

#[test]
fn map_url_uses_flashvars_host_and_base_path() {
    assert_eq!(
        map_config_url("https://int1.darkorbit.com/", "", "/spacemap/")
            .unwrap()
            .as_str(),
        "https://int1.darkorbit.com/spacemap/xml/maps.php"
    );
    assert_eq!(
        map_config_url(
            "https://int1.darkorbit.com/",
            "//dynamic.example/",
            "spacemap"
        )
        .unwrap()
        .as_str(),
        "https://dynamic.example/spacemap/xml/maps.php"
    );
}

#[test]
fn main_swf_url_uses_cdn_and_gameclient_hash() {
    let mut vars = std::collections::HashMap::new();
    assert!(main_swf_url(&vars).is_none());
    vars.insert("cdn".into(), "https://darkorbit-22.bpsecure.com/".into());
    vars.insert(
        "gameclientHash".into(),
        "689a7d5b371e6480830f2ead31e39800".into(),
    );
    vars.insert("gameclientPath".into(), "spacemap".into());
    assert_eq!(
        main_swf_url(&vars).unwrap().as_str(),
        "https://darkorbit-22.bpsecure.com/spacemap/main.swf?__cv=689a7d5b371e6480830f2ead31e39800"
    );
}

#[test]
fn version_hash_from_compressed_swf() {
    use std::io::Write;
    let hash = b"bd171f1aac49f58dde9ed1e00e539553";
    let mut payload = b"padVERSION_HASH ".to_vec();
    payload.extend_from_slice(hash);
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&payload).unwrap();
    let compressed = encoder.finish().unwrap();
    let mut swf = b"CWS\x15".to_vec();
    swf.extend_from_slice(&(8 + payload.len() as u32).to_le_bytes());
    swf.extend(compressed);
    assert_eq!(
        version_hash_from_swf(&swf).as_deref(),
        Some("bd171f1aac49f58dde9ed1e00e539553")
    );
}

fn fixture() -> (ControllerPacketClient, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let control = stream.try_clone().unwrap();
    let mut session = PacketSession::new(stream);
    session.world.logged_in = true;
    session.world.hero = Some(HeroState {
        user_id: 42,
        x: 100,
        y: 200,
        hitpoints: 500.0,
        hitpoints_max: 1000.0,
        ..Default::default()
    });
    session.world.entities.insert(
        5,
        Entity {
            id: 5,
            name: "Another player".into(),
            ..Default::default()
        },
    );
    session.world.entities.insert(
        9,
        Entity {
            id: 9,
            npc: true,
            name: "NPC".into(),
            x: 300,
            y: 400,
            ..Default::default()
        },
    );
    session.world.collectables.insert(
        "box-hash".into(),
        Collectable {
            hash: "box-hash".into(),
            kind: CollectableKind::Box,
            x: 110,
            y: 210,
            type_id: 1,
            type_str: String::new(),
        },
    );
    (
        ControllerPacketClient {
            inner: Arc::new(PacketGameClient::new(session)),
            control,
            stopped: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(None)),
            selected: Mutex::new(None),
            boxes: Mutex::new(BTreeMap::new()),
        },
        peer,
    )
}

#[test]
fn snapshot_identifies_hero_and_stable_collectable_ids() {
    let (client, _peer) = fixture();
    let first = client.snapshot(None).unwrap();
    let hero = first
        .entities
        .iter()
        .find(|e| e.category == EntityCategory::Player)
        .unwrap();
    assert_eq!(hero.id, Some(json!(42)));
    assert_eq!(hero.health, Some(500.0));
    assert_eq!(
        first
            .entities
            .iter()
            .filter(|e| e.category == EntityCategory::Player)
            .count(),
        1
    );
    let boxx = first
        .entities
        .iter()
        .find(|e| e.name == "box-hash")
        .unwrap();
    assert_eq!(boxx.fields["entityKind"], "box");
    assert_eq!(boxx.fields["typeID"], 1);
    assert!(boxx.id.as_ref().unwrap().as_i64().unwrap() < 0);
    let second = client.snapshot(None).unwrap();
    assert_eq!(
        second
            .entities
            .iter()
            .find(|e| e.name == "box-hash")
            .unwrap()
            .id,
        boxx.id
    );
    let frame = xenomit_game_api::LiveFrame::from_game_snapshot(&first);
    assert_eq!(frame.hero.unwrap().id, 42);
}

#[test]
fn controller_calls_produce_packet_requests() {
    use xenomit_packet_client::{frame::stream::read_packet, modules::id};
    let (client, mut peer) = fixture();
    client
        .call_function(
            &FunctionCall::new("event", "moveHeroToCordinates")
                .with_arguments(vec![json!(1000), json!(2000)])
                .in_frame(),
        )
        .unwrap();
    assert_eq!(read_packet(&mut peer, true).unwrap().id, id::MOVE_REQUEST);
    client
        .call_function(&FunctionCall::new("ship", "selectShip").with_arguments(vec![json!(9)]))
        .unwrap();
    assert_eq!(
        read_packet(&mut peer, true).unwrap().id,
        id::SHIP_SELECT_REQUEST
    );
    client
        .call_function(
            &FunctionCall::new("facade", "sendNotification")
                .with_arguments(vec![json!("MapAssetNotificationAttackMapAsset")]),
        )
        .unwrap();
    assert_eq!(
        read_packet(&mut peer, true).unwrap().id,
        id::ATTACK_LASER_REQUEST
    );
    let snapshot = client.snapshot(None).unwrap();
    let box_id = snapshot
        .entities
        .iter()
        .find(|e| e.name == "box-hash")
        .unwrap()
        .id
        .clone()
        .unwrap();
    client
        .call_function(
            &FunctionCall::new("event", "moveHeroToCordinates").with_arguments(vec![
                json!(110),
                json!(210),
                json!({"$xenomit_entity_id":box_id}),
            ]),
        )
        .unwrap();
    assert_eq!(
        read_packet(&mut peer, true).unwrap().id,
        id::COLLECT_BOX_REQUEST
    );
}

#[test]
fn map_server_xml_is_read_from_gzip() {
    use std::io::Write;
    let xml = "<maps><map id='6'><gameserverIP>gameserver.example:6062</gameserverIP></map></maps>";
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(xml.as_bytes()).unwrap();
    let text = decode_http_body(&encoder.finish().unwrap()).unwrap();
    assert_eq!(map_endpoint(&text, "6").unwrap(), "gameserver.example:6062");
}

#[test]
fn selects_current_map_server() {
    let xml = "<maps><map id='1'><gameserverIP>one:6000</gameserverIP></map><map id='2'><gameserverIP>two</gameserverIP></map></maps>";
    assert_eq!(map_endpoint(xml, "1").unwrap(), "one:6000");
    assert_eq!(map_endpoint(xml, "2").unwrap(), "two:5001");
    assert!(map_endpoint(xml, "3").is_err());
}
