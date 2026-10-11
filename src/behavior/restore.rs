use super::*;

/// Capture the module + rule state for a snapshot or periodic checkpoint.
pub(super) fn capture_runtime_checkpoint(
    runner: &ModuleRunner,
    behaviors: &Mutex<BehaviorRunner>,
    now_ms: u64,
    quality: CheckpointQuality,
    snapshot: Option<&GameSnapshot>,
) -> RuntimeCheckpoint {
    let best_effort = quality != CheckpointQuality::Clean;
    let module = runner.export_state(now_ms, best_effort);
    let rules = behaviors
        .lock()
        .map(|runner| runner.export_state(now_ms))
        .unwrap_or(Value::Null);
    let mut issues = module
        .as_ref()
        .map(|state| state.issues.clone())
        .unwrap_or_default();
    if module.is_none() {
        issues.push("module exported no state; the successor starts it fresh".into());
    }
    let game_hint = snapshot.map(|s| {
        let hero = s
            .entities
            .iter()
            .find(|e| e.category == xenomit_game_api::EntityCategory::Player);
        serde_json::json!({
            "map_id": s.navigation.map_id,
            "hero_x": hero.map(|e| e.position.x as i64),
            "hero_y": hero.map(|e| e.position.y as i64),
        })
    });
    RuntimeCheckpoint {
        schema: RuntimeCheckpoint::SCHEMA,
        module_id: Some(runner.descriptor().id.clone()),
        module,
        rules,
        run_elapsed_ms: now_ms,
        game_hint,
        issues,
    }
}

/// Apply a pending restore at worker start. `started` is shifted back by the
/// checkpoint's run time so exported ages rebase onto this run's clock.
/// Returns Err only when a `Strict` policy meets an incompatible checkpoint.
pub(super) fn apply_pending_restore(
    restore: PendingRestore,
    runner: &mut ModuleRunner,
    behaviors: &Mutex<BehaviorRunner>,
    started: &mut Instant,
    report: &Mutex<Option<String>>,
) -> Result<(), String> {
    if let Some(shifted) = Instant::now()
        .checked_sub(Duration::from_millis(restore.checkpoint.run_elapsed_ms))
    {
        *started = shifted;
    }
    let now_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let checkpoint = &restore.checkpoint;
    if checkpoint.schema > RuntimeCheckpoint::SCHEMA {
        return finish_restore(
            Err(format!(
                "checkpoint schema {} is newer than supported {}",
                checkpoint.schema,
                RuntimeCheckpoint::SCHEMA
            )),
            &restore,
        );
    }
    let result = match (&checkpoint.module_id, &checkpoint.module) {
        (Some(id), Some(state)) if *id == runner.descriptor().id => {
            runner.restore_state(state, now_ms).map_err(|e| e.to_string())
        }
        (Some(id), _) => Err(format!(
            "checkpoint module {id} does not match the configured module {}",
            runner.descriptor().id
        )),
        (None, _) => Ok(()),
    };
    let rule_issues = behaviors
        .lock()
        .map(|mut runner| runner.restore_state(&checkpoint.rules, now_ms))
        .unwrap_or_else(|_| vec!["behavior runner lock poisoned".into()]);
    for issue in checkpoint.issues.iter().chain(rule_issues.iter()) {
        publish(report, format!("Restore issue: {issue}"));
    }
    finish_restore(result, &restore)
}

pub(super) fn finish_restore(result: Result<(), String>, restore: &PendingRestore) -> Result<(), String> {
    let strict = restore.policy == RestorePolicy::Strict
        && restore.quality == CheckpointQuality::Clean;
    match (result, strict) {
        (Err(error), true) => Err(format!("snapshot {}: {error}", describe_restore(restore))),
        (Err(error), false) => {
            // Degraded checkpoints and best-effort policy start fresh.
            let _ = error;
            Ok(())
        }
        (Ok(()), _) => Ok(()),
    }
}

pub(super) fn describe_restore(restore: &PendingRestore) -> String {
    restore
        .snapshot_id
        .clone()
        .unwrap_or_else(|| "pending".into())
}

/// Shared interruption permission ladder for client refresh and handoff
/// drain: general-behavior takeovers, in-flight rule services, the current
/// pickup, the current attack, then the module's own safe point.
#[allow(clippy::too_many_arguments)]
pub(super) fn interruption_permission(
    runner: &ModuleRunner,
    owns_plan: bool,
    rule_operations: &[xenomit_behavior_engine::OperationHandle],
    executor: &ActionExecutor,
    snapshot: Option<&GameSnapshot>,
    ready: bool,
    now_ms: u64,
    reason: InterruptionReason,
) -> InterruptionStatus {
    if owns_plan {
        InterruptionStatus::Deferred("Waiting for the active general behavior to finish".into())
    } else if !rule_operations.is_empty() {
        InterruptionStatus::Deferred("Waiting for the active behavior service to finish".into())
    } else if executor
        .collecting_until
        .is_some_and(|until| Instant::now() < until)
    {
        InterruptionStatus::Deferred("Waiting for the current pickup to finish".into())
    } else if executor.active_npc.as_ref().is_some_and(|key| {
        snapshot.as_ref().is_some_and(|s| {
            s.entities.iter().any(|e| {
                let entity_key = e
                    .id
                    .as_ref()
                    .map(|id| format!("id:{id}"))
                    .unwrap_or_else(|| format!("path:{}", e.path));
                &entity_key == key && e.health.is_none_or(|hp| hp > 0.0)
            })
        })
    }) {
        InterruptionStatus::Deferred("Waiting for the current attack to finish".into())
    } else {
        runner.interruption_status(
            &InterruptionContext {
                now_ms,
                snapshot,
                game_ready: ready && snapshot.is_some(),
            },
            reason,
        )
    }
}
