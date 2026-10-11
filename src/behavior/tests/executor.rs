use super::*;

#[test]
fn kill_collect_executes_pickup_and_preserves_it_on_the_next_frame() {
    let client = collecting_client(true);
    let mut planner =
        BehaviorEngine::new(configured_behavior(&module("kill_collect")).unwrap()).unwrap();
    let mut executor = ActionExecutor::default();
    let first = step_behavior(&client, &mut planner, &mut executor).unwrap();
    assert!(first.contains("Collecting"), "{first}");
    assert_eq!(client.calls("selectShip").len(), 1);
    let moves = client.calls("moveHeroToCordinates");
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].arguments.len(), 3);
    assert_eq!(moves[0].timing, CallTiming::InFrame);
    client.snapshot.lock().unwrap().observed_at_unix_ms = 2;
    let second = step_behavior(&client, &mut planner, &mut executor).unwrap();
    assert!(second.contains("Waiting for collection"), "{second}");
    assert_eq!(client.calls("moveHeroToCordinates").len(), 1);
    assert!(client.calls("stopHero").is_empty());
}

#[test]
fn collection_retries_use_engine_timing_instead_of_a_second_five_second_timer() {
    let client = collecting_client(false);
    let mut planner =
        BehaviorEngine::new(configured_behavior(&module("collect")).unwrap()).unwrap();
    let mut executor = ActionExecutor::default();
    step_behavior(&client, &mut planner, &mut executor).unwrap();
    client.snapshot.lock().unwrap().observed_at_unix_ms = 1700;
    step_behavior(&client, &mut planner, &mut executor).unwrap();
    assert_eq!(client.calls("moveHeroToCordinates").len(), 2);
}

#[test]
fn disappeared_box_releases_wait_and_allows_another_pickup() {
    let client = collecting_client(false);
    let mut planner =
        BehaviorEngine::new(configured_behavior(&module("collect")).unwrap()).unwrap();
    let mut executor = ActionExecutor::default();
    step_behavior(&client, &mut planner, &mut executor).unwrap();
    let mut next = client.snapshot.lock().unwrap();
    next.observed_at_unix_ms = 2;
    next.entities[1].id = Some(json!(4));
    next.entities[1].path = "stage.new_box".into();
    drop(next);
    step_behavior(&client, &mut planner, &mut executor).unwrap();
    assert_eq!(client.calls("moveHeroToCordinates").len(), 2);
    assert_eq!(
        client.calls("moveHeroToCordinates")[1].arguments[2],
        json!({"$xenomit_entity_id": 4})
    );
}

#[test]
fn failed_collection_call_releases_planner_wait() {
    let client = collecting_client(false);
    let mut planner =
        BehaviorEngine::new(configured_behavior(&module("collect")).unwrap()).unwrap();
    let mut executor = ActionExecutor::default();
    client.fail_collection.store(true, Ordering::Release);
    assert!(step_behavior(&client, &mut planner, &mut executor).is_err());
    client.fail_collection.store(false, Ordering::Release);
    let line = step_behavior(&client, &mut planner, &mut executor).unwrap();
    assert!(line.contains("Collecting"), "{line}");
    assert_eq!(client.calls("moveHeroToCordinates").len(), 2);
}

#[test]
fn working_map_plan_suspends_ordinary_work_instead_of_roaming() {
    let client = collecting_client(false);
    client.snapshot.lock().unwrap().entities.push(entity(
        EntityCategory::Portal,
        "portal",
        400.0,
        100.0,
        9,
    ));
    let mut cfg = configured_behavior(&module("collect")).unwrap();
    cfg.working_map = Some(2);
    let mut planner = BehaviorEngine::new(cfg).unwrap();
    let mut executor = ActionExecutor::default();
    let line = step_behavior(&client, &mut planner, &mut executor).unwrap();
    assert!(line.contains("Traveling to working map 2"), "{line}");
    assert!(client.calls("moveHeroToCordinates").is_empty());
}

#[test]
fn runtime_jumps_to_selected_map_then_resumes_retained_module() {
    use xenomit_game_api::{MapDescriptor, MapLink, PortalInfo, StarSystem};
    let mut snapshot = snapshot(vec![entity(
        EntityCategory::Player,
        "hero",
        100.0,
        100.0,
        1,
    )]);
    snapshot.navigation.map_id = Some(1);
    snapshot.navigation.geometry_ready = true;
    snapshot.navigation.star_system = StarSystem {
        maps: vec![
            MapDescriptor {
                id: 1,
                name: "1-1".into(),
                min_level: 0,
            },
            MapDescriptor {
                id: 2,
                name: "1-2".into(),
                min_level: 0,
            },
        ],
        links: vec![MapLink {
            from: 1,
            to: 2,
            portal_id: 12,
        }],
    };
    snapshot.navigation.portals = vec![PortalInfo {
        id: 12,
        type_id: 0,
        position: ApiPoint { x: 100.0, y: 100.0 },
        target_map: Some(2),
        jumping: false,
    }];
    let client = Arc::new(FakeClient::new(snapshot));
    let runtime = PlannerRuntime::default();
    runtime
        .start(
            client.clone(),
            ModuleConfig {
                module_id: "collect".into(),
                settings: json!({"working_map":2,"tick_ms":10,"safety":{"enabled":false}}),
            },
        )
        .unwrap();
    wait_for(|| {
        client.commands.lock().unwrap().iter().any(|command| {
            matches!(
                command,
                xenomit_game_api::GameCommand::JumpPortal { portal_id: 12 }
            )
        })
    });
    // Settlement is cooperative, with no duplicate jump after map changes.
    wait_for(|| {
        runtime
            .report()
            .is_some_and(|status| status.contains("roaming"))
    });
    assert_eq!(
        client
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|command| matches!(
                command,
                xenomit_game_api::GameCommand::JumpPortal { .. }
            ))
            .count(),
        1
    );
    assert!(runtime.is_running());
    runtime.stop().unwrap();
}

#[test]
fn executor_routes_movement_around_obstacles() {
    let mut snapshot = snapshot(vec![entity(
        EntityCategory::Player,
        "hero",
        100.0,
        1000.0,
        1,
    )]);
    snapshot.navigation.map_id = Some(1);
    snapshot.navigation.geometry_ready = true;
    snapshot.navigation.obstacles = vec![xenomit_game_api::Obstacle {
        id: -1,
        name: "NOA".into(),
        inverted: false,
        shape: xenomit_game_api::ObstacleShape::Rectangle {
            bounds: Bounds {
                x: 700.0,
                y: 650.0,
                width: 600.0,
                height: 700.0,
            },
        },
    }];
    let client = FakeClient::new(snapshot.clone());
    let mut executor = ActionExecutor::default();
    executor
        .routed_move(
            &client,
            &snapshot,
&json!({}),
            xenomit_behavior_engine::Point {
                x: 100.0,
                y: 1000.0,
            },
            xenomit_behavior_engine::Point {
                x: 1900.0,
                y: 1000.0,
            },
        )
        .unwrap();
    let calls = client.calls("moveHeroToCordinates");
    let arguments = &calls[0].arguments;
    assert_ne!(arguments, &vec![json!(1900), json!(1000)]);
    assert!(
        ObstacleNavigation::from_snapshot(&snapshot, CoordinateTransform::default())
            .segment_clear(
                xenomit_behavior_engine::Point {
                    x: 100.0,
                    y: 1000.0
                },
                xenomit_behavior_engine::Point {
                    x: arguments[0].as_f64().unwrap(),
                    y: arguments[1].as_f64().unwrap()
                }
            )
    );
    snapshot.navigation.geometry_ready = false;
    assert!(
        executor
            .routed_move(
                &client,
                &snapshot,
&json!({}),
                xenomit_behavior_engine::Point {
                    x: 100.0,
                    y: 1000.0
                },
                xenomit_behavior_engine::Point {
                    x: 1900.0,
                    y: 1000.0
                }
            )
            .is_err()
    );
    assert_eq!(client.calls("moveHeroToCordinates").len(), 1);
}

#[test]
fn module_configs_are_valid() {
    for id in ["kill_collect", "npc_killer", "collect", "palladium"] {
        configured_behavior(&module(id)).unwrap_or_else(|error| panic!("module {id}: {error}"));
    }
    assert!(configured_behavior(&module("nonexistent")).is_err());
}

#[test]
fn in_range_npc_is_selected_and_attacked() {
    let client = FakeClient::new(snapshot(vec![
        entity(EntityCategory::Player, "hero", 100.0, 100.0, 1),
        entity(EntityCategory::Npc, "streuner", 500.0, 100.0, 42),
    ]));
    let (mut planner, mut executor) = npc_killer(&client);

    let line = step_behavior(&client, &mut planner, &mut executor).unwrap();

    assert!(!line.contains("roaming"), "{line}");
    assert_eq!(client.calls("selectShip").len(), 1);
    assert_eq!(client.calls("sendNotification").len(), 1);
    assert_eq!(client.calls("selectShip")[0].timing, CallTiming::Immediate);
}

#[test]
fn npc_species_map_restricts_the_target_list() {
    let client = FakeClient::new(snapshot(vec![
        entity(EntityCategory::Player, "hero", 100.0, 100.0, 1),
        entity(EntityCategory::Npc, "streuner", 500.0, 100.0, 42),
        entity(EntityCategory::Npc, "lordakia", 600.0, 100.0, 43),
    ]));
    let mut restricted = module("npc_killer");
    restricted.settings = json!({"npcs": {"lordakia": true, "streuner": false}});
    let mut planner = BehaviorEngine::new(configured_behavior(&restricted).unwrap()).unwrap();
    let mut executor = ActionExecutor::default();

    step_behavior(&client, &mut planner, &mut executor).unwrap();

    // Only lordakia may be selected — streuner is closer but filtered out.
    let selects = client.calls("selectShip");
    assert_eq!(selects.len(), 1);
    assert_eq!(selects[0].arguments[0], json!(43));
}

#[test]
fn npc_with_unreadable_health_is_still_targeted() {
    // 0/0 means the snapshot lacks HP data — it is not a corpse.
    let mut unread = entity(EntityCategory::Npc, "streuner", 500.0, 100.0, 42);
    unread.health = Some(0.0);
    unread.max_health = Some(0.0);
    let client = FakeClient::new(snapshot(vec![
        entity(EntityCategory::Player, "hero", 100.0, 100.0, 1),
        unread,
    ]));
    let (mut planner, mut executor) = npc_killer(&client);

    step_behavior(&client, &mut planner, &mut executor).unwrap();

    assert_eq!(client.calls("selectShip").len(), 1);
}

#[test]
fn dead_npc_is_not_targeted() {
    let mut dead = entity(EntityCategory::Npc, "streuner", 500.0, 100.0, 42);
    dead.health = Some(0.0);
    dead.max_health = Some(100.0);
    let client = FakeClient::new(snapshot(vec![
        entity(EntityCategory::Player, "hero", 100.0, 100.0, 1),
        dead,
    ]));
    let (mut planner, mut executor) = npc_killer(&client);

    let line = step_behavior(&client, &mut planner, &mut executor).unwrap();

    assert_eq!(client.calls("selectShip").len(), 0);
    assert!(line.contains("no NPC"), "{line}");
}

#[test]
fn species_rules_match_across_name_forms() {
    // The kill list may store the qualified class name while the frame
    // carries the short species name — or the reverse.
    let mut npc = entity(EntityCategory::Npc, "Streuner", 500.0, 100.0, 42);
    npc.class_name = "net.bigpoint.darkorbit.assets::Streuner".into();
    let client = FakeClient::new(snapshot(vec![
        entity(EntityCategory::Player, "hero", 100.0, 100.0, 1),
        npc,
    ]));
    let mut restricted = module("npc_killer");
    restricted.settings = json!({"npcs": {"net.bigpoint.darkorbit.assets::Streuner": true}});
    let mut planner = BehaviorEngine::new(configured_behavior(&restricted).unwrap()).unwrap();
    let mut executor = ActionExecutor::default();

    step_behavior(&client, &mut planner, &mut executor).unwrap();

    assert_eq!(client.calls("selectShip").len(), 1);
}

#[test]
fn all_species_disabled_targets_no_npc() {
    let client = FakeClient::new(snapshot(vec![
        entity(EntityCategory::Player, "hero", 100.0, 100.0, 1),
        entity(EntityCategory::Npc, "streuner", 500.0, 100.0, 42),
    ]));
    let mut spared = module("npc_killer");
    spared.settings = json!({"npcs": {"streuner": false}});
    let mut planner = BehaviorEngine::new(configured_behavior(&spared).unwrap()).unwrap();
    let mut executor = ActionExecutor::default();

    step_behavior(&client, &mut planner, &mut executor).unwrap();

    assert_eq!(client.calls("selectShip").len(), 0);
}

#[test]
fn out_of_range_npc_roams_instead_of_idling() {
    let client = FakeClient::new(snapshot(vec![
        entity(EntityCategory::Player, "hero", 100.0, 100.0, 1),
        entity(EntityCategory::Npc, "streuner", 15_000.0, 100.0, 42),
    ]));
    let (mut planner, mut executor) = npc_killer(&client);

    let line = step_behavior(&client, &mut planner, &mut executor).unwrap();

    assert!(line.contains("roaming"), "{line}");
    // The ship must receive an actual movement order toward the patrol.
    let moves = client.calls("moveHeroToCordinates");
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].timing, CallTiming::InFrame);
}

#[test]
fn missing_player_is_reported_without_moving() {
    let client = FakeClient::new(snapshot(vec![entity(
        EntityCategory::Npc,
        "streuner",
        500.0,
        100.0,
        42,
    )]));
    let (mut planner, mut executor) = npc_killer(&client);

    let line = step_behavior(&client, &mut planner, &mut executor).unwrap();

    assert!(line.contains("player entity"), "{line}");
    assert!(client.functions().is_empty());
}

#[test]
fn unready_bridge_reports_and_does_nothing() {
    let mut client = FakeClient::new(snapshot(vec![entity(
        EntityCategory::Player,
        "hero",
        100.0,
        100.0,
        1,
    )]));
    client.ready = false;
    let (mut planner, mut executor) = npc_killer(&client);

    let line = step_behavior(&client, &mut planner, &mut executor).unwrap();

    assert!(line.contains("bridge"), "{line}");
    assert!(client.functions().is_empty());
}

#[test]
fn roam_advances_to_the_next_waypoint_on_arrival() {
    let client = FakeClient::new(snapshot(vec![entity(
        EntityCategory::Player,
        "hero",
        10_500.0,
        6_550.0,
        1,
    )]));
    let (mut planner, mut executor) = npc_killer(&client);

    step_behavior(&client, &mut planner, &mut executor).unwrap();
    let moves = client.calls("moveHeroToCordinates");
    assert_eq!(moves.len(), 1);
    // The player is at the map center (first patrol point), so the ship
    // must be sent onward to a quadrant waypoint, not told to stand still.
    assert_eq!(moves[0].arguments, vec![json!(5250), json!(3275)]);
}
