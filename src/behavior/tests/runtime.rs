use super::*;

#[test]
fn overdue_refresh_keeps_eternal_gate_running_and_refreshes_before_next_run() {
    let _serial = REFRESH_TESTS.lock().unwrap();
    take_host_actions();
    let mut state = snapshot(vec![entity(
        EntityCategory::Player,
        "hero",
        100.0,
        100.0,
        1,
    )]);
    state.navigation.map_id = Some(-4);
    state.navigation.geometry_ready = true;
    state.map.as_mut().unwrap().id = Some(-4);
    state.map.as_mut().unwrap().name = Some("GG Eternal".into());
    let client = Arc::new(FakeClient::new(state));
    let config = module("eternal_blacklight");
    let stop = CancellationToken::default();
    let runner = xenomit_behavior_engine::builtin_registry()
        .create(config.clone(), stop.clone())
        .unwrap();
    let report = Arc::new(Mutex::new(None));
    let worker_report = report.clone();
    let worker_client = client.clone();
    let worker_stop = stop.clone();
    let thread = thread::spawn(move || {
        run_loop(
            worker_client,
            runner,
            ServiceRegistry::default(),
            worker_stop,
            Duration::from_millis(10),
            worker_report,
            Arc::new(Mutex::new(None)),
            overdue_refresh_rules(),
            config.settings,
            json!({}),
            Instant::now() - Duration::from_secs(61),
            Arc::new(HostActionQueue),
            None,
        )
    });
    wait_for(|| {
        report
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|line| line.contains("periodic refresh pending"))
    });
    assert!(
        take_host_actions().is_empty(),
        "An empty wave inside EBG cannot authorize refresh"
    );
    {
        let mut snapshot = client.snapshot.lock().unwrap();
        snapshot.navigation.map_id = Some(1);
        snapshot.map.as_mut().unwrap().id = Some(1);
        snapshot.map.as_mut().unwrap().name = Some("1-1".into());
    }
    wait_for(|| {
        report
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|line| line.contains("Refreshing client at a safe module boundary"))
    });
    wait_for(|| {
        HOST_ACTIONS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .contains(&HostAction::Refresh)
    });
    assert_eq!(take_host_actions(), vec![HostAction::Refresh]);
    assert!(!client.commands.lock().unwrap().iter().any(|command|matches!(command,xenomit_game_api::GameCommand::UseItem {item_id,..} if item_id.contains("eternal-blacklight-cpu"))),"Must intercept before starting another gate");
    stop.cancel();
    thread.thread().unpark();
    thread.join().unwrap().unwrap();
}

#[test]
fn completion_refresh_does_not_replay_a_completed_sequence() {
    let _serial = REFRESH_TESTS.lock().unwrap();
    take_host_actions();
    let mut registry = xenomit_behavior_engine::ModuleRegistryBuilder::new();
    registry
        .register(custom_descriptor(&[]), |_| {
            Ok(Box::new(xenomit_behavior_engine::TaskSequence::new()))
        })
        .unwrap();
    let stop = CancellationToken::default();
    let config = module("custom");
    let runner = registry
        .build()
        .create(config.clone(), stop.clone())
        .unwrap();
    let client = Arc::new(FakeClient::new(snapshot(vec![entity(
        EntityCategory::Player,
        "hero",
        100.0,
        100.0,
        1,
    )])));
    run_loop(
        client,
        runner,
        ServiceRegistry::default(),
        stop,
        Duration::from_millis(10),
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(None)),
        overdue_refresh_rules(),
        config.settings,
        json!({}),
        Instant::now() - Duration::from_secs(61),
        Arc::new(HostActionQueue),
        None,
    )
    .unwrap();
    assert_eq!(take_host_actions(), vec![HostAction::RefreshIdle]);
}

#[test]
fn plugin_selection_rejects_invalid_activation_values() {
    let mut config = module("collect");
    config.settings = json!({"plugins":{"base":"false"}});
    assert!(plugin_selection(&config).unwrap_err().contains("boolean"));
    config.settings = json!({"plugins":[]});
    assert!(plugin_selection(&config).is_err());
    config.settings = json!({});
    assert!(plugin_selection(&config).unwrap().is_empty());
}

#[test]
fn active_worker_rejects_plugin_activation_updates() {
    let runtime = PlannerRuntime::with_catalog(
        PluginCatalog::builtin(),
        vec![],
        ServiceRegistry::default(),
    );
    let mut client = FakeClient::new(snapshot(vec![]));
    client.ready = false;
    let mut config = module("collect");
    config.settings = json!({"tick_ms":60000});
    runtime.start(Arc::new(client), config.clone()).unwrap();
    config.settings["plugins"] = json!({"base":false});
    runtime.update(&config);
    assert!(
        runtime
            .report()
            .unwrap()
            .contains("Plugin settings cannot change")
    );
    assert!(runtime.pending.lock().unwrap().is_none());
    assert!(runtime.is_running());
    runtime.stop().unwrap();
}

#[test]
fn disabled_plugin_rejects_objective_before_worker_creation() {
    let runtime = PlannerRuntime::with_catalog(
        PluginCatalog::builtin(),
        vec![],
        ServiceRegistry::default(),
    );
    let mut config = module("galaxy");
    config.settings = json!({"plugins":{"gg":false}});
    let error = runtime
        .start(Arc::new(FakeClient::new(snapshot(vec![]))), config)
        .unwrap_err();
    assert!(error.contains("unknown module galaxy"), "{error}");
    assert!(!runtime.is_running());
}

#[test]
fn disabling_plugin_removes_stopped_rules_and_rebuilds_the_retained_session() {
    let runtime = PlannerRuntime::with_catalog(
        PluginCatalog::builtin(),
        vec![],
        ServiceRegistry::default(),
    );
    let mut state = snapshot(vec![entity(
        EntityCategory::Player,
        "hero",
        100.0,
        100.0,
        1,
    )]);
    state.navigation.hero.items.insert(
        "test-item".into(),
        xenomit_game_api::ItemState {
            available: true,
            activatable: true,
            quantity: Some(1.0),
            ..Default::default()
        },
    );
    let client = Arc::new(FakeClient::new(state.clone()));
    let mut config = module("collect");
    config.settings = json!({"plugins":{"utility":false},"behaviors":{"custom_events":{"enabled":true,"tick_stopped":true,"options":{"events":[{"item":"test-item","condition":{},"cooldown_ms":1000}]}}}});
    runtime
        .tick_stopped(client.clone(), &config, Some(&state))
        .unwrap();
    assert!(client.commands.lock().unwrap().is_empty());
    config.settings["plugins"]["utility"] = json!(true);
    runtime
        .tick_stopped(client.clone(), &config, Some(&state))
        .unwrap();
    assert_eq!(client.commands.lock().unwrap().len(), 1);
    config.settings["plugins"]["utility"] = json!(false);
    runtime
        .tick_stopped(client.clone(), &config, Some(&state))
        .unwrap();
    assert_eq!(client.commands.lock().unwrap().len(), 1);
}

#[test]
fn custom_sequence_runs_services_while_game_is_loading_and_finishes() {
    use xenomit_behavior_engine::{
        ModuleRegistryBuilder, OperationRequest, RetryPolicy, ServiceRegistryBuilder,
        TaskSequence,
    };
    let (sent, received) = std::sync::mpsc::channel();
    let mut services = ServiceRegistryBuilder::new();
    services
        .register("gate.spin", move |_, value| {
            sent.send(value).unwrap();
            Ok(json!({"gate": "alpha"}))
        })
        .unwrap();
    let mut registry = ModuleRegistryBuilder::new();
    registry
        .register(custom_descriptor(&["gate.spin"]), |_| {
            Ok(Box::new(TaskSequence::new().operation(
                "spin",
                OperationRequest::service("gate.spin", json!({"count":10})),
                Duration::from_secs(30),
                RetryPolicy::default(),
            )?))
        })
        .unwrap();
    let runtime = PlannerRuntime::with_registry(registry.build(), services.build());
    let mut client = FakeClient::new(snapshot(vec![]));
    client.ready = false;
    let mut config = module("custom");
    config.settings = json!({"tick_ms":10});
    assert_eq!(runtime.modules().unwrap()[0].id, "custom");
    runtime.start(Arc::new(client), config).unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(3)).unwrap(),
        json!({"count":10})
    );
    wait_for(|| !runtime.is_running());
    assert_eq!(runtime.report().as_deref(), Some("Sequence completed"));
    runtime.stop().unwrap();
}

#[test]
fn stop_wakes_long_tick_and_cancels_inflight_services() {
    use xenomit_behavior_engine::{
        ModuleRegistryBuilder, OperationRequest, RetryPolicy, ServiceRegistryBuilder,
        TaskSequence,
    };
    let (sent, received) = std::sync::mpsc::channel();
    let (cancelled, observed) = std::sync::mpsc::channel();
    let mut services = ServiceRegistryBuilder::new();
    services
        .register("event.wait", move |context, _| {
            sent.send(()).unwrap();
            while !context.cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            cancelled.send(()).unwrap();
            Ok(Value::Null)
        })
        .unwrap();
    let mut registry = ModuleRegistryBuilder::new();
    registry
        .register(custom_descriptor(&["event.wait"]), |_| {
            Ok(Box::new(TaskSequence::new().operation(
                "event",
                OperationRequest::service("event.wait", Value::Null),
                Duration::from_secs(12 * 3600),
                RetryPolicy::default(),
            )?))
        })
        .unwrap();
    let runtime = PlannerRuntime::with_registry(registry.build(), services.build());
    let mut config = module("custom");
    config.settings = json!({"tick_ms":60_000});
    runtime
        .start(Arc::new(FakeClient::new(snapshot(vec![]))), config)
        .unwrap();
    received.recv_timeout(Duration::from_secs(3)).unwrap();
    let stopped = Instant::now();
    runtime.stop().unwrap();
    assert!(stopped.elapsed() < Duration::from_secs(2));
    observed.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(!runtime.is_running());
}

#[test]
fn missing_service_fails_before_worker_start() {
    let mut registry = xenomit_behavior_engine::ModuleRegistryBuilder::new();
    registry
        .register(custom_descriptor(&["missing.spin"]), |_| {
            Ok(Box::new(xenomit_behavior_engine::TaskSequence::new()))
        })
        .unwrap();
    let runtime = PlannerRuntime::with_registry(registry.build(), ServiceRegistry::default());
    let error = runtime
        .start(
            Arc::new(FakeClient::new(snapshot(vec![]))),
            module("custom"),
        )
        .unwrap_err();
    assert!(error.contains("missing.spin"));
    assert!(!runtime.is_running());
}

#[test]
fn behavior_takeover_preserves_and_resumes_one_objective_instance() {
    use std::sync::atomic::AtomicUsize;
    use xenomit_behavior_engine::{
        BehaviorModule, GeneralBehavior, ModuleOutput, RuleOutput, TickContext,
    };
    let constructions = Arc::new(AtomicUsize::new(0));
    let ticks = Arc::new(AtomicUsize::new(0));
    let allow = Arc::new(AtomicBool::new(false));
    struct Objective(Arc<AtomicUsize>);
    impl BehaviorModule for Objective {
        fn tick(&mut self, _: &mut TickContext<'_>) -> ModuleOutput {
            self.0.fetch_add(1, Ordering::SeqCst);
            ModuleOutput::running("Objective")
        }
    }
    struct Takeover(Arc<AtomicBool>);
    impl GeneralBehavior for Takeover {
        fn tick(&mut self, context: &RuleContext<'_>) -> RuleOutput {
            RuleOutput {
                takeover: (!self.0.load(Ordering::SeqCst))
                    .then(|| idle_rule_plan(context.snapshot.unwrap())),
                ..Default::default()
            }
        }
        fn update(&mut self, _: &Value) -> Result<(), String> {
            Ok(())
        }
    }
    let mut modules = xenomit_behavior_engine::ModuleRegistryBuilder::new();
    let built = constructions.clone();
    let observed = ticks.clone();
    modules
        .register(custom_descriptor(&[]), move |_| {
            built.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Objective(observed.clone())))
        })
        .unwrap();
    let mut rules = BehaviorRegistry::default();
    let allowed = allow.clone();
    rules
        .register(
            0,
            xenomit_game_api::ModuleDescriptor::new("test_rule", "Test rule"),
            move |_| Ok(Box::new(Takeover(allowed.clone()))),
        )
        .unwrap();
    let runtime = PlannerRuntime::with_behavior_registry(
        modules.build(),
        rules,
        ServiceRegistry::default(),
    );
    let mut config = module("custom");
    config.settings = json!({"tick_ms":10});
    runtime
        .start(
            Arc::new(FakeClient::new(snapshot(vec![entity(
                EntityCategory::Player,
                "hero",
                100.0,
                100.0,
                1,
            )]))),
            config,
        )
        .unwrap();
    wait_for(|| {
        runtime
            .report()
            .is_some_and(|s| s.contains("Applying general behavior"))
    });
    assert_eq!(ticks.load(Ordering::SeqCst), 0);
    allow.store(true, Ordering::SeqCst);
    wait_for(|| ticks.load(Ordering::SeqCst) > 0);
    runtime.stop().unwrap();
    assert_eq!(constructions.load(Ordering::SeqCst), 1);
}

#[test]
fn stopped_rules_execute_only_after_explicit_opt_in() {
    let runtime = PlannerRuntime::default();
    let mut state = snapshot(vec![entity(
        EntityCategory::Player,
        "hero",
        100.0,
        100.0,
        1,
    )]);
    state.navigation.hero.items.insert(
        "test-item".into(),
        xenomit_game_api::ItemState {
            available: true,
            activatable: true,
            quantity: Some(1.0),
            ..Default::default()
        },
    );
    let client = Arc::new(FakeClient::new(state.clone()));
    let mut config = module("collect");
    config.settings = json!({"behaviors":{"custom_events":{"enabled":true,"options":{"events":[{"item":"test-item","condition":{},"cooldown_ms":1000}]}}}});
    runtime
        .tick_stopped(client.clone(), &config, Some(&state))
        .unwrap();
    assert!(client.commands.lock().unwrap().is_empty());
    config.settings["behaviors"]["custom_events"]["tick_stopped"] = json!(true);
    runtime
        .tick_stopped(client.clone(), &config, Some(&state))
        .unwrap();
    assert_eq!(client.commands.lock().unwrap().len(), 1);
    runtime
        .tick_stopped(client.clone(), &config, Some(&state))
        .unwrap();
    assert_eq!(
        client.commands.lock().unwrap().len(),
        1,
        "stopped ticks preserve cooldown state"
    );
}

#[test]
fn unavailable_quest_service_rejects_rule_before_worker_start() {
    let runtime = PlannerRuntime::default();
    let mut config = module("collect");
    config.settings =
        json!({"behaviors":{"urgent_detector":{"enabled":true,"options":{"take_quest":true}}}});
    let error = runtime
        .start(Arc::new(FakeClient::new(snapshot(vec![]))), config)
        .unwrap_err();
    assert!(error.contains("game.urgent_quest"));
    assert!(!runtime.is_running());
}

#[test]
fn safe_stop_leaves_gate_even_when_objective_is_paused() {
    let runtime = PlannerRuntime::default();
    let mut state = snapshot(vec![entity(
        EntityCategory::Player,
        "hero",
        100.0,
        100.0,
        1,
    )]);
    state.entities[0].fields.insert("isGG".into(), json!(true));
    state.navigation.map_id = Some(1);
    state.navigation.geometry_ready = true;
    state.navigation.star_system.maps = vec![
        xenomit_game_api::MapDescriptor {
            id: 1,
            name: "GG alpha".into(),
            min_level: 1,
        },
        xenomit_game_api::MapDescriptor {
            id: 2,
            name: "1-1".into(),
            min_level: 1,
        },
    ];
    state.navigation.portals = vec![xenomit_game_api::PortalInfo {
        id: 7,
        position: xenomit_game_api::Point {
            x: 1000.0,
            y: 100.0,
        },
        type_id: 0,
        target_map: Some(2),
        jumping: false,
    }];
    let client = Arc::new(FakeClient::new(state.clone()));
    let mut config = module("collect");
    config.settings = json!({"behaviors":{"stop_button":{"enabled":true,"tick_stopped":true,"options":{"requested":true}}}});
    runtime
        .tick_stopped(client.clone(), &config, Some(&state))
        .unwrap();
    assert!(
        client
            .functions()
            .iter()
            .any(|f| f == "moveHeroToCordinates")
    );
    state.entities[0].position.x = 1000.0;
    state.entities[0]
        .fields
        .insert("worldX".into(), json!(1000.0));
    runtime
        .tick_stopped(client.clone(), &config, Some(&state))
        .unwrap();
    assert!(
        client
            .commands
            .lock()
            .unwrap()
            .contains(&xenomit_game_api::GameCommand::JumpPortal { portal_id: 7 })
    );
}

#[test]
fn running_module_rejects_switch_without_resetting_its_sequence() {
    let constructions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = constructions.clone();
    let mut registry = xenomit_behavior_engine::ModuleRegistryBuilder::new();
    registry
        .register(custom_descriptor(&[]), move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(
                xenomit_behavior_engine::TaskSequence::new()
                    .delay("event", Duration::from_secs(12 * 3600)),
            ))
        })
        .unwrap();
    let runtime = PlannerRuntime::with_registry(registry.build(), ServiceRegistry::default());
    let mut config = module("custom");
    config.settings = json!({"tick_ms":10});
    runtime
        .start(Arc::new(FakeClient::new(snapshot(vec![]))), config)
        .unwrap();
    runtime.update(&module("collect"));
    wait_for(|| {
        runtime
            .report()
            .is_some_and(|line| line.contains("settings rejected"))
    });
    assert!(runtime.is_running());
    assert_eq!(constructions.load(Ordering::Relaxed), 1);
    assert_eq!(runtime.modules().unwrap()[0].id, "custom");
    runtime.stop().unwrap();
}

#[test]
fn custom_module_panic_clears_worker_liveness() {
    struct Panics {
        engine: BehaviorEngine,
        first: bool,
    }
    impl xenomit_behavior_engine::BehaviorModule for Panics {
        fn tick(
            &mut self,
            context: &mut xenomit_behavior_engine::TickContext<'_>,
        ) -> xenomit_behavior_engine::ModuleOutput {
            if self.first {
                self.first = false;
                return xenomit_behavior_engine::ModuleOutput {
                    state: RunState::Running,
                    status: "Combat before panic".into(),
                    plan: Some(self.engine.step(context.snapshot.unwrap(), context.now_ms)),
                };
            }
            panic!("custom module crashed")
        }
    }
    let mut registry = xenomit_behavior_engine::ModuleRegistryBuilder::new();
    registry
        .register(custom_descriptor(&[]), |_| {
            Ok(Box::new(Panics {
                engine: BehaviorEngine::new(configured_behavior(&module("npc_killer"))?)
                    .map_err(|e| e.to_string())?,
                first: true,
            }))
        })
        .unwrap();
    let runtime = PlannerRuntime::with_registry(registry.build(), ServiceRegistry::default());
    let client = Arc::new(FakeClient::new(snapshot(vec![
        entity(EntityCategory::Player, "hero", 100.0, 100.0, 1),
        entity(EntityCategory::Npc, "streuner", 500.0, 100.0, 42),
    ])));
    let mut config = module("custom");
    config.settings = json!({"tick_ms":10});
    runtime.start(client.clone(), config).unwrap();
    wait_for(|| {
        runtime
            .report()
            .is_some_and(|line| line.contains("panicked"))
    });
    assert!(!runtime.is_running());
    assert!(!client.calls("moveHeroToCordinates").is_empty());
    assert_eq!(client.calls("stopHero").len(), 1);
    assert_eq!(client.calls("deselectSelectedShip").len(), 1);
    assert!(runtime.stop().is_err());
}
