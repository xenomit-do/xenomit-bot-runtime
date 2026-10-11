use super::*;
use xenomit_game_api::live::LiveMap;
use xenomit_game_api::{MapDescriptor, MapLink, PortalInfo, StarSystem};

fn test_frame() -> LiveFrame {
    let mut frame = LiveFrame {
        captured_unix_ms: 1_700_000_000_000,
        map: Some(LiveMap {
            id: 8,
            width: 21_000.0,
            height: 13_100.0,
        }),
        hero: Some(xenomit_game_api::live::LiveHero {
            id: 42,
            x: 1000.0,
            y: 2000.0,
            dest: Some(Point { x: 5.0, y: 6.0 }),
            faction: 1,
            clan: 0,
        }),
        ..Default::default()
    };
    frame.navigation.hero.configuration = Some(2);
    frame.navigation.hero.level = Some(17);
    frame.navigation.hero.owns_pet = true;
    frame.navigation.hero.pet_active = true;
    frame.navigation.star_system = StarSystem {
        maps: vec![
            MapDescriptor {
                id: 8,
                name: "1-8".into(),
                min_level: 0,
            },
            MapDescriptor {
                id: 1,
                name: "1-1".into(),
                min_level: 0,
            },
        ],
        links: vec![MapLink {
            from: 8,
            to: 1,
            portal_id: 11,
        }],
    };
    frame.navigation.portals.push(PortalInfo {
        id: 11,
        position: Point { x: 100.0, y: 200.0 },
        type_id: 1,
        target_map: Some(1),
        jumping: false,
    });
    frame.entities.push(LiveEntity {
        id: 42,
        kind: LiveKind::Hero,
        x: 1000.0,
        y: 2000.0,
        hp: 90_000,
        max_hp: 100_000,
        shield: 50_000,
        max_shield: 80_000,
        nanohull: 400,
        nanohull_max: 500,
        faction: 1,
        name: "TestPilot".into(),
        is_hero: true,
        has_hp: true,
        ..Default::default()
    });
    frame.entities.push(LiveEntity {
        id: 77,
        kind: LiveKind::Npc,
        x: 1500.0,
        y: 1600.0,
        hp: 500,
        max_hp: 500,
        faction: 0,
        clan_diplomacy: 3,
        name: "Streuner".into(),
        is_npc: true,
        has_hp: true,
        ..Default::default()
    });
    frame
}

#[test]
fn frame_serializes_to_protocol_v1() {
    let payload = frame_payload(&test_frame());
    assert_eq!(payload["map"]["id"], 8);
    assert_eq!(payload["map"]["name"], "1-8");
    assert_eq!(payload["map"]["width"], 21_000.0);
    let hero = &payload["hero"];
    assert_eq!(hero["id"], 42);
    assert_eq!(hero["hp"], 90_000);
    assert_eq!(hero["nanohull"], 400);
    assert_eq!(hero["configuration"], 2);
    assert_eq!(hero["faction"], "mmo");
    assert_eq!(hero["level"], 17);
    assert_eq!(hero["pet"]["owned"], true);
    assert_eq!(hero["pet"]["active"], true);
    let npc = payload["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == 77)
        .unwrap();
    assert_eq!(npc["kind"], "npc");
    assert_eq!(npc["clan_diplomacy"], "war");
    assert_eq!(payload["portals"][0]["target_name"], "1-1");
    assert_eq!(payload["portals"][0]["jumping"], false);
    // No session material may ever leak into frames.
    let text = serde_json::to_string(&payload).unwrap();
    for banned in ["sid", "dosid", "session_id", "cookie"] {
        assert!(
            !text.to_lowercase().contains(banned),
            "frame leaks {banned}"
        );
    }
}

#[test]
fn module_settings_flattens_descriptor_paths() {
    let mut descriptor = xenomit_game_api::ModuleDescriptor::new("m", "M");
    descriptor
        .settings
        .push(xenomit_game_api::SettingDescriptor {
            key: "safety.min_health".into(),
            label: "Min".into(),
            description: String::new(),
            group: "Safety".into(),
            kind: xenomit_game_api::SettingKind::Integer { min: 0, max: 100 },
            default: json!(40),
        });
    descriptor
        .settings
        .push(xenomit_game_api::SettingDescriptor {
            key: "npcs".into(),
            label: "NPCs".into(),
            description: String::new(),
            group: "Targets".into(),
            kind: xenomit_game_api::SettingKind::BooleanMap { discover: None },
            default: json!({}),
        });
    let nested = json!({
        "safety": {"min_health": 55},
        "npcs": {"Streuner": true},
        "other_injected": true,
    });
    let flat = module_settings_flat(&descriptor, &nested);
    assert_eq!(flat["safety.min_health"], 55);
    assert_eq!(flat["npcs"], json!({"Streuner": true}));
    assert!(!flat.contains_key("other_injected"));
    // Missing keys fall back to the descriptor default.
    let flat = module_settings_flat(&descriptor, &json!({}));
    assert_eq!(flat["safety.min_health"], 40);
}

/// Live-panel integration: requires PANEL_URL + PANEL_TOKEN (or
/// PANEL_CODE to exercise pairing first). Run explicitly with
/// `cargo test -- --ignored panel_live`.
#[test]
#[ignore]
fn panel_live_connects_sends_hello_frame_status_and_answers_ping() {
    let url = std::env::var("PANEL_URL").expect("PANEL_URL");
    let token = match std::env::var("PANEL_TOKEN") {
        Ok(token) => token,
        Err(_) => {
            let code = std::env::var("PANEL_CODE").expect("PANEL_CODE");
            let response = pair(&url, &code, json!({"server": "int1.darkorbit.com"}))
                .expect("pairing failed");
            assert!(!response.token.is_empty());
            response.token
        }
    };
    let link = Arc::new(PanelLink::default());
    link.configure(PanelSettings {
        enabled: true,
        url,
        token,
        ..Default::default()
    });
    link.set_hello(json!({
        "instance": {"server": "int1.darkorbit.com", "user_id": "42", "username": "LiveTest"},
        "client": "ruffle",
        "backend": "native",
        "modules": [{"id": "kill_collect", "name": "Kill & Collect", "settings": []}],
        "config": {"active_module": "kill_collect", "modules": {}},
    }));
    link.set_status(json!({
        "bot_running": false,
        "module_id": "kill_collect",
        "activity": "live test",
        "uptime_s": 1,
        "client": "ruffle",
        "backend": "native",
        "balances": [1.0, 2.0, 3.0, 4.0],
        "rates": {"cre_h": 0.0, "uri_h": 0.0, "exp_h": 0.0, "hon_h": 0.0},
    }));
    link.push_frame(&test_frame());
    spawn_agent(link.clone());
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && link.state().0 != "online" {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        link.state().0,
        "online",
        "agent failed to connect: {}",
        link.state().1
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(link.state().0, "online");
    link.shutdown();
}

#[test]
fn ws_url_accepts_http_https_and_bare_hosts() {
    assert_eq!(
        ws_url("http://localhost:3000").unwrap(),
        "ws://localhost:3000/api/agent/ws"
    );
    assert_eq!(
        ws_url("https://dash.example.com/").unwrap(),
        "wss://dash.example.com/api/agent/ws"
    );
    assert_eq!(
        ws_url("dash.example.com").unwrap(),
        "wss://dash.example.com/api/agent/ws"
    );
    assert_eq!(
        ws_url("ws://127.0.0.1:3000/").unwrap(),
        "ws://127.0.0.1:3000/api/agent/ws"
    );
}
