use super::*;

pub(super) fn idle_rule_plan(snapshot: &GameSnapshot) -> BehaviorDecision {
    BehaviorDecision {
        mode: xenomit_behavior_engine::BehaviorMode::Collector,
        player: snapshot
            .entities
            .iter()
            .find(|e| e.category == xenomit_game_api::EntityCategory::Player)
            .map(|e| xenomit_behavior_engine::EntityRef {
                path: e.path.clone(),
                id: e.id.clone(),
                name: e.name.clone(),
                class_name: e.class_name.clone(),
                position: xenomit_behavior_engine::Point {
                    x: e.position.x,
                    y: e.position.y,
                },
            }),
        selected_npc: None,
        selected_box: None,
        actions: vec![],
        note: Some("Applying general behavior".into()),
        controls: Default::default(),
        suspended: true,
        can_refresh: false,
    }
}

pub(super) fn select_rule_target(
    client: &dyn GameClient,
    snapshot: &GameSnapshot,
    id: i32,
) -> Result<(), String> {
    let Some(entity) = snapshot
        .entities
        .iter()
        .find(|e| e.id.as_ref().and_then(Value::as_i64) == Some(i64::from(id)))
    else {
        return Err("Group member is no longer in the snapshot".into());
    };
    let call = FunctionCall::new(SHIP_MANAGER, "selectShip").with_arguments(vec![
        json!(id),
        json!(entity.health.unwrap_or(0.0)),
        json!(entity.max_health.unwrap_or(0.0)),
        json!(entity.shield.unwrap_or(0.0)),
        json!(entity.max_shield.unwrap_or(0.0)),
        json!(0),
        json!(0),
    ]);
    invoke(client, call, "select group member").map(|_| ())
}

#[cfg(test)]
pub(super) fn configured_behavior(module: &ModuleConfig) -> Result<BehaviorConfig, String> {
    xenomit_behavior_engine::builtin_behavior_config(module)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_loop(
    client: Arc<dyn GameClient>,
    mut runner: ModuleRunner,
    services: ServiceRegistry,
    stop: CancellationToken,
    mut tick: Duration,
    report: Arc<Mutex<Option<String>>>,
    pending: Arc<Mutex<Option<ModuleConfig>>>,
    behaviors: Arc<Mutex<BehaviorRunner>>,
    mut module_settings: Value,
    mut behavior_settings: Value,
    mut started: Instant,
    host_actions: Arc<dyn HostActionHandler>,
    handoff: Option<Arc<HandoffLink>>,
) -> Result<(), String> {
    let mut cleanup = ActionCleanup {
        client: &*client,
        executor: ActionExecutor::default(),
    };
    let executor = &mut cleanup.executor;
    let mut operations = ThreadedOperationHost::new(services, stop.clone());
    let mut settings_error = None;
    let mut rule_operations = Vec::new();
    // Apply a checkpoint restore before the first tick so the module resumes
    // at its step boundary rather than replaying completed work.
    if let Some(restore) = handoff.as_ref().and_then(|link| link.take_restore()) {
        let result = apply_pending_restore(restore, &mut runner, &behaviors, &mut started, &report);
        if let Some(link) = &handoff {
            link.post_restore_result(result.clone());
        }
        result?;
    }
    let mut drain: Option<DrainState> = None;
    let mut last_drain_reason: Option<String> = None;
    while !stop.is_cancelled() {
        rule_operations.retain(|handle| match operations.poll(*handle) {
            OperationPoll::Pending => true,
            OperationPoll::Succeeded(_) => {
                operations.release(*handle);
                false
            }
            OperationPoll::Failed(error) => {
                publish(&report, format!("Behavior service failed: {error}"));
                operations.release(*handle);
                false
            }
        });
        if let Some(module) = pending.lock().ok().and_then(|mut slot| slot.take()) {
            let incoming = module
                .settings
                .get("behaviors")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if let Err(error) = behaviors
                .lock()
                .map_err(|_| "Behavior lock was poisoned")?
                .update(&incoming)
            {
                settings_error = Some(error);
                thread::park_timeout(tick);
                continue;
            }
            match runner.update(&module) {
                Ok(()) => {
                    module_settings = module.settings.clone();
                    behavior_settings = incoming;
                    settings_error = None;
                    if let Some(ms) = module.settings.get("tick_ms").and_then(Value::as_u64) {
                        tick = Duration::from_millis(ms.clamp(10, 60_000));
                    }
                }
                Err(error) => settings_error = Some(error),
            }
        }
        let state = client.status().map_err(|error| error.to_string());
        let ready = state.as_ref().is_ok_and(|status| status.ready);
        let snapshot = if ready {
            client
                .snapshot(None)
                .map(|mut snapshot| {
                    normalize_world_positions(&mut snapshot);
                    apply_global_target_constraints(&mut snapshot, &module_settings);
                    snapshot
                })
                .ok()
        } else {
            None
        };
        let now_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let rule_output = behaviors
            .lock()
            .map_err(|_| "Behavior lock was poisoned")?
            .tick(&RuleContext {
                now_ms,
                snapshot: snapshot.as_ref(),
                module_settings: &module_settings,
                stopped: snapshot.as_ref().is_none_or(|s| s.navigation.hero.killed),
            });
        let owns_plan = rule_output.takeover.is_some();
        let refresh_requested = rule_output
            .host_actions
            .contains(&HostAction::RequestRefresh)
            && !rule_output.stop;
        let mut refresh_deferred = None;
        if refresh_requested {
            let permission = interruption_permission(
                &runner,
                owns_plan,
                &rule_operations,
                executor,
                snapshot.as_ref(),
                ready,
                now_ms,
                InterruptionReason::ClientRefresh,
            );
            match permission {
                InterruptionStatus::Ready => {
                    // Check before ticking: an overdue refresh must not start a new gate/workflow.
                    executor.stop(&*client)?;
                    publish(&report, "Refreshing client at a safe module boundary");
                    host_actions.handle(HostAction::Refresh);
                    // Keep liveness until the controller consumes the request, records resume
                    // intent and stops us. No further module or service ticks are issued.
                    while !stop.is_cancelled() {
                        thread::park_timeout(Duration::from_millis(250));
                    }
                    return Ok(());
                }
                InterruptionStatus::Deferred(reason) => refresh_deferred = Some(reason),
            }
        }
        // Handoff drain: keep working until the shared ladder reports a safe
        // point, then stop movement/targeting and hand state to the daemon.
        // Non-interruptible work pauses the drain deadline; the hard deadline
        // always bounds the total wait (docs/03 §3.2).
        if let Some(link) = &handoff {
            if drain.is_none()
                && let Some(directive) = link.drain()
            {
                publish(
                    &report,
                    format!("Draining for handoff: {}", directive.reason),
                );
                drain = Some(DrainState::new(directive));
            }
            if let Some(active) = &mut drain {
                let permission = interruption_permission(
                    &runner,
                    owns_plan,
                    &rule_operations,
                    executor,
                    snapshot.as_ref(),
                    ready,
                    now_ms,
                    InterruptionReason::Handoff,
                );
                active.tick(runner.non_interruptible());
                let conclude = if active.hard_deadline_passed() {
                    Some((CheckpointQuality::Degraded, "hard deadline"))
                } else {
                    match permission {
                        InterruptionStatus::Ready => {
                            Some((CheckpointQuality::Clean, "safe point"))
                        }
                        InterruptionStatus::Deferred(reason) => {
                            active.note_deferred(&reason);
                            if last_drain_reason.as_deref() != Some(reason.as_str()) {
                                publish(
                                    &report,
                                    format!("Handoff drain pending: {reason}"),
                                );
                                last_drain_reason = Some(reason);
                            }
                            active
                                .drain_deadline_passed()
                                .then_some((CheckpointQuality::Degraded, "drain deadline"))
                        }
                    }
                };
                if let Some((quality, concluded)) = conclude {
                    let _ = executor.stop(&*client);
                    let checkpoint = capture_runtime_checkpoint(
                        &runner,
                        &behaviors,
                        now_ms,
                        quality,
                        snapshot.as_ref(),
                    );
                    link.post_drained(crate::handoff::Drained {
                        quality,
                        checkpoint,
                        wait_reasons: std::mem::take(&mut active.wait_reasons),
                        concluded,
                    });
                    return Ok(());
                }
            } else if link.checkpoint_due() {
                link.post_checkpoint(capture_runtime_checkpoint(
                    &runner,
                    &behaviors,
                    now_ms,
                    CheckpointQuality::Checkpoint,
                    snapshot.as_ref(),
                ));
            }
        }
        let filtered = snapshot.as_ref().map(|s| {
            let mut s = behaviors.lock().expect("behavior lock").module_snapshot(s);
            filter_booty_boxes(&mut s, &behavior_settings);
            s
        });
        let mut output = if let Some(plan) = rule_output.takeover {
            xenomit_behavior_engine::ModuleOutput {
                state: RunState::Running,
                status: plan.note.clone().unwrap_or_default(),
                plan: Some(plan),
            }
        } else {
            runner.tick(
                now_ms,
                filtered.as_ref(),
                ready && snapshot.is_some(),
                &mut operations,
            )
        };
        if !rule_output.commands.is_empty() && output.plan.is_none() {
            output.plan = snapshot.as_ref().map(idle_rule_plan);
        }
        if let Some(plan) = &mut output.plan {
            if owns_plan && let Some(snapshot) = &snapshot {
                if let Some(enabled) = plan.controls.pet_enabled {
                    let pet = &snapshot.navigation.hero;
                    if enabled != pet.pet_active
                        && (!enabled || (pet.owns_pet && !pet.pet_destroyed))
                    {
                        plan.controls
                            .commands
                            .insert(0, xenomit_game_api::GameCommand::SetPet { enabled });
                    }
                }
                if let Some(mode) = plan.controls.ship_mode {
                    let key = match mode {
                        xenomit_behavior_engine::ShipMode::Attack => "offensive_config",
                        xenomit_behavior_engine::ShipMode::Run => "run_config",
                        xenomit_behavior_engine::ShipMode::Roam => "roam_config",
                    };
                    if let Some(configuration) = module_settings
                        .get(key)
                        .and_then(Value::as_u64)
                        .filter(|c| matches!(*c, 1 | 2))
                        && snapshot.navigation.hero.configuration != Some(configuration as u8)
                    {
                        plan.controls.commands.insert(
                            0,
                            xenomit_game_api::GameCommand::SelectConfiguration {
                                configuration: configuration as u8,
                            },
                        );
                    }
                }
            }
            plan.controls.commands.extend(rule_output.commands);
            // Avoid duplicate item commands from module managers and general rules.
            let mut seen = std::collections::BTreeSet::new();
            plan.controls.commands.reverse();
            plan.controls.commands.retain(|command| {
                if let xenomit_game_api::GameCommand::UseItem { item_id, .. } = command {
                    let slot = [
                        "ammunition_laser_",
                        "ammunition_rocketlauncher_",
                        "drone_formation_",
                    ]
                    .into_iter()
                    .find(|prefix| item_id.starts_with(prefix))
                    .map(str::to_owned)
                    .unwrap_or_else(|| item_id.clone());
                    seen.insert(slot)
                } else {
                    true
                }
            });
            plan.controls.commands.reverse();
        }
        match output.state {
            RunState::Completed => {
                publish(&report, output.status);
                if refresh_requested {
                    // Completed workflows may refresh, but must not be restarted/repeated.
                    host_actions.handle(HostAction::RefreshIdle);
                }
                return executor.stop(&*client);
            }
            RunState::Cancelled => break,
            RunState::Failed(error) => {
                let _ = executor.stop(&*client);
                return Err(error);
            }
            RunState::Running => {}
        }
        if let (Some(plan), Some(snapshot)) = (&output.plan, &snapshot) {
            executor.now_ms = now_ms;
            let commands = plan.controls.commands.iter().try_for_each(|command| {
                if matches!(command, xenomit_game_api::GameCommand::JumpPortal { .. }) {
                    executor.stop(&*client)?;
                }
                client
                    .game_command(command)
                    .map_err(|error| error.to_string())?;
                behaviors
                    .lock()
                    .map_err(|_| "Behavior lock was poisoned")?
                    .command_succeeded(command, now_ms);
                if !owns_plan {
                    runner.command_succeeded(command, now_ms);
                }
                Ok::<(), String>(())
            });
            if let Err(error) = commands {
                let _ = executor.stop(&*client);
                if owns_plan {
                    behaviors
                        .lock()
                        .map_err(|_| "Behavior lock was poisoned")?
                        .plan_failed();
                } else {
                    runner.plan_failed(plan);
                }
                publish(&report, format!("Game command failed: {error}"));
                thread::park_timeout(tick);
                continue;
            }
            match execute_decision(&*client, snapshot, plan, executor, &module_settings) {
                Ok(line) => {
                    if executor.collection_deferred {
                        if owns_plan {
                            behaviors
                                .lock()
                                .map_err(|_| "Behavior lock was poisoned")?
                                .plan_failed();
                        } else {
                            runner.plan_failed(plan);
                        }
                    } else {
                        if !owns_plan {
                            runner.plan_succeeded(plan, now_ms);
                        }
                    }
                    publish(&report, line);
                }
                Err(error) => {
                    let _ = executor.stop(&*client);
                    if owns_plan {
                        behaviors
                            .lock()
                            .map_err(|_| "Behavior lock was poisoned")?
                            .plan_failed();
                    } else {
                        runner.plan_failed(plan);
                    }
                    publish(&report, format!("{error}; retrying"));
                }
            }
        } else {
            // A disconnected/loading bridge must not continue the last attack
            // or movement order on an old map while a module is waiting.
            if snapshot.is_none() {
                let _ = executor.stop(&*client);
            }
            publish(&report, output.status);
        }
        for action in rule_output.host_actions {
            match action {
                HostAction::RequestRefresh => {} // handled cooperatively before module tick
                HostAction::SelectTarget(id) => {
                    if let Some(entity) = snapshot.as_ref().and_then(|s| {
                        s.entities
                            .iter()
                            .find(|e| e.id.as_ref().and_then(Value::as_i64) == Some(i64::from(id)))
                    }) {
                        let call =
                            FunctionCall::new(SHIP_MANAGER, "selectShip").with_arguments(vec![
                                json!(id),
                                json!(entity.health.unwrap_or(0.0)),
                                json!(entity.max_health.unwrap_or(0.0)),
                                json!(entity.shield.unwrap_or(0.0)),
                                json!(entity.max_shield.unwrap_or(0.0)),
                                json!(0),
                                json!(0),
                            ]);
                        if let Err(error) = invoke(&*client, call, "select group member") {
                            publish(&report, error);
                        }
                    }
                }
                HostAction::AcceptUrgentQuest | HostAction::ClaimCalendar => {
                    let service = if action == HostAction::AcceptUrgentQuest {
                        "game.urgent_quest"
                    } else {
                        "backpage.calendar"
                    };
                    match operations.start(xenomit_behavior_engine::OperationRequest::service(
                        service,
                        json!({}),
                    )) {
                        Ok(handle) => rule_operations.push(handle),
                        Err(error) => publish(&report, error),
                    }
                }
                action => {
                    host_actions.handle(action);
                }
            }
        }
        if !rule_output.status.is_empty() {
            let current = report
                .lock()
                .ok()
                .and_then(|v| v.clone())
                .unwrap_or_default();
            publish(
                &report,
                format!("{current}; {}", rule_output.status.join("; ")),
            );
        }
        if rule_output.stop {
            executor.stop(&*client)?;
            return Ok(());
        }
        if let Some(reason) = refresh_deferred {
            let current = report
                .lock()
                .ok()
                .and_then(|line| line.clone())
                .unwrap_or_default();
            publish(
                &report,
                format!("{current}; periodic refresh pending: {reason}"),
            );
        }
        if let Some(error) = &settings_error {
            let status = report
                .lock()
                .ok()
                .and_then(|line| line.clone())
                .unwrap_or_default();
            publish(&report, format!("{status}; settings rejected: {error}"));
        }
        // stop() unparks this thread; long task delays never block cancellation.
        thread::park_timeout(tick);
    }
    executor.stop(&*client)
}

#[cfg(test)]
pub(super) fn step_behavior(
    client: &dyn GameClient,
    planner: &mut BehaviorEngine,
    executor: &mut ActionExecutor,
) -> Result<String, String> {
    let status = client.status().map_err(|error| error.to_string())?;
    if !status.ready {
        return Ok("Waiting for the game bridge to become ready".into());
    }
    let mut snapshot = client
        .snapshot(None)
        .map_err(|error| format!("could not read game state: {error}"))?;
    normalize_world_positions(&mut snapshot);
    let decision = planner.step(&snapshot, snapshot.observed_at_unix_ms);
    let result = execute_decision(client, &snapshot, &decision, executor, &Value::Null);
    if (result.is_err() || executor.collection_deferred)
        && let Some(target) = &decision.selected_box
    {
        planner.collection_failed(target);
    }
    result
}

pub(super) fn execute_decision(
    client: &dyn GameClient,
    snapshot: &GameSnapshot,
    decision: &BehaviorDecision,
    executor: &mut ActionExecutor,
    settings: &Value,
) -> Result<String, String> {
    executor.collection_deferred = false;
    let Some(player) = decision.player.clone() else {
        executor.collecting_until = None;
        executor.sync_target(client, None, Some(snapshot))?;
        executor.stop_moving(client)?;
        if snapshot.navigation.hero.killed
            && let Some(note) = &decision.note
        {
            return Ok(note.clone());
        }
        return Ok(format!(
            "No player entity in the game snapshot ({} entities read)",
            snapshot.entities.len()
        ));
    };

    let move_to = decision
        .actions
        .iter()
        .filter_map(|action| match action {
            ActionIntent::MoveTo { destination } => Some(*destination),
            _ => None,
        })
        .next_back();
    let collect = decision.actions.iter().find_map(|action| match action {
        ActionIntent::CollectBox { target, wait_ms } => Some((target, *wait_ms)),
        _ => None,
    });
    if collect.is_none()
        && !decision
            .actions
            .iter()
            .any(|action| matches!(action, ActionIntent::WaitForCollection { .. }))
    {
        executor.collecting_until = None;
    }

    executor.sync_target(client, decision.selected_npc.as_ref(), Some(snapshot))?;

    // Collection owns movement in combined mode; the engine requests attack
    // alongside pickup without an orbit order that would cancel collection.
    let collect_now = collect.filter(|_| move_to.is_none());
    if let Some((target, wait_ms)) = collect_now {
        // A vanished previous box must not leave an executor-level wait/retry
        // blocking the engine's newly selected box.
        let navigation =
            ObstacleNavigation::from_snapshot(snapshot, CoordinateTransform::default());
        let approach = navigation
            .nearest_point(target.position)
            .ok_or("Box has no walkable approach")?;
        if !navigation.segment_clear(player.position, approach) {
            executor.routed_move(client, snapshot, settings, player.position, approach)?;
            // The planner started its pickup wait on this attempt. Release it
            // so it keeps following the route until a direct pickup is safe.
            executor.collection_deferred = true;
            return Ok("Routing around obstacles toward collection target".into());
        }
        executor.collect_at(client, target, approach, wait_ms)?;
    }
    if let Some(destination) = move_to {
        executor.routed_move(client, snapshot, settings, player.position, destination)?;
        let target = decision
            .selected_npc
            .as_ref()
            .or(decision.selected_box.as_ref())
            .map(|target| format!(" toward {}", entity_label(target)))
            .unwrap_or_default();
        Ok(format!(
            "Moving{target} to {:.0},{:.0}",
            destination.x, destination.y
        ))
    } else if let Some((target, _)) = collect_now {
        Ok(format!("Collecting {}", entity_label(target)))
    } else if decision.suspended {
        if decision
            .actions
            .iter()
            .any(|action| matches!(action, ActionIntent::WaitForCollection { .. }))
        {
            return Ok(format!(
                "Waiting for collection{}",
                decision
                    .selected_box
                    .as_ref()
                    .map(|target| format!(" of {}", entity_label(target)))
                    .unwrap_or_default()
            ));
        }
        executor.collecting_until = None;
        executor.stop_moving(client)?;
        Ok(decision
            .note
            .clone()
            .unwrap_or_else(|| "Module suspended".into()))
    } else if let Some(npc) = decision.selected_npc.as_ref() {
        Ok(format!("Attacking {}", entity_label(npc)))
    } else {
        // The plan is empty — no NPC or box is in range. Keep searching the
        // map instead of sitting inert, so modules hunt their targets down.
        let zone_targets = zone_roam_targets(snapshot, settings);
        if executor.roam(client, player.position, snapshot, settings, &zone_targets)? {
            Ok(match decision.note.clone() {
                Some(note) => format!("{note}; roaming the map"),
                None => "Roaming the map".into(),
            })
        } else {
            Ok(decision
                .note
                .clone()
                .unwrap_or_else(|| "Idle — no target in range".into()))
        }
    }
}
