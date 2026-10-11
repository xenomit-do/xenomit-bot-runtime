use super::*;

#[derive(Default)]
pub(super) struct ActionExecutor {
    pub(super) last_attack: Option<Instant>,
    pub(super) router: MovementRouter,
    pub(super) now_ms: u64,
    pub(super) collection_deferred: bool,
    pub(super) active_npc: Option<String>,
    pub(super) moving: bool,
    pub(super) last_move: Option<(f64, f64, Instant)>,
    pub(super) collecting_until: Option<Instant>,
    pub(super) roam_index: usize,
    pub(super) roam_target: Option<(f64, f64)>,
}

/// Stop previously issued actions even when a custom tick unwinds. Keep this
/// outside module-owned state so its panic cannot leave the last order active.
pub(super) struct ActionCleanup<'a> {
    pub(super) client: &'a dyn GameClient,
    pub(super) executor: ActionExecutor,
}

impl Drop for ActionCleanup<'_> {
    fn drop(&mut self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.executor.stop(self.client)
        }));
    }
}

impl ActionExecutor {
    pub(super) fn routed_move(
        &mut self,
        client: &dyn GameClient,
        snapshot: &GameSnapshot,
        settings: &Value,
        from: xenomit_behavior_engine::Point,
        destination: xenomit_behavior_engine::Point,
    ) -> Result<(), String> {
        let destination = clip_destination_away_from_zones(snapshot, settings, from, destination);
        match self.router.next(snapshot, from, destination, self.now_ms)? {
            Some(next) => self.move_to(client, next),
            None => {
                self.collecting_until = None;
                self.stop_moving(client)
            }
        }
    }
    pub(super) fn sync_target(
        &mut self,
        client: &dyn GameClient,
        selected: Option<&xenomit_behavior_engine::EntityRef>,
        snapshot: Option<&GameSnapshot>,
    ) -> Result<(), String> {
        match selected {
            Some(target) => {
                let key = target.key();
                if self.active_npc.as_deref() == Some(key.as_str()) {
                    let hero = snapshot.and_then(|snapshot| {
                        snapshot.entities.iter().find(|entity| {
                            entity.category == xenomit_game_api::EntityCategory::Player
                        })
                    });
                    let confirmed = hero.and_then(|hero| hero.fields.get("attackingTargetId"));
                    let retry = confirmed
                        .is_some_and(|attacking| Some(attacking) != target.id.as_ref())
                        && self
                            .last_attack
                            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
                    if !retry {
                        return Ok(());
                    }
                }
                let id = entity_id(target)?;
                let entity = snapshot.and_then(|snapshot| {
                    snapshot.entities.iter().find(|entity| {
                        entity.id.as_ref() == target.id.as_ref() || entity.path == target.path
                    })
                });
                let call = FunctionCall::new(SHIP_MANAGER, "selectShip").with_arguments(vec![
                    json!(id),
                    json!(entity.and_then(|entity| entity.health).unwrap_or(0.0)),
                    json!(entity.and_then(|entity| entity.max_health).unwrap_or(0.0)),
                    json!(entity.and_then(|entity| entity.shield).unwrap_or(0.0)),
                    json!(entity.and_then(|entity| entity.max_shield).unwrap_or(0.0)),
                    json!(
                        entity
                            .and_then(|entity| numeric_field(entity, "nanohullHP"))
                            .unwrap_or(0.0)
                    ),
                    json!(
                        entity
                            .and_then(|entity| numeric_field(entity, "nanohullMaxHP"))
                            .unwrap_or(0.0)
                    ),
                ]);
                invoke(client, call, "select NPC")?;
                let call = FunctionCall::new(GAME_FACADE, "sendNotification")
                    .with_arguments(vec![json!("MapAssetNotificationAttackMapAsset")]);
                invoke(client, call, "start NPC attack")?;
                self.active_npc = Some(key);
                self.last_attack = Some(Instant::now());
            }
            None => {
                if self.active_npc.take().is_some() {
                    invoke(
                        client,
                        FunctionCall::new(SHIP_MANAGER, "deselectSelectedShip"),
                        "stop NPC attack",
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn move_to(
        &mut self,
        client: &dyn GameClient,
        destination: xenomit_behavior_engine::Point,
    ) -> Result<(), String> {
        let now = Instant::now();
        self.moving = true;
        self.collecting_until = None;
        let should_send = self.last_move.is_none_or(|(x, y, sent)| {
            now.duration_since(sent) >= MOVE_REFRESH
                || (x - destination.x).hypot(y - destination.y) >= MOVE_REFRESH_DISTANCE
        });
        if should_send {
            invoke(
                client,
                movement_call(destination.x, destination.y)?,
                "move to behavior target",
            )?;
            self.last_move = Some((destination.x, destination.y, now));
        }
        Ok(())
    }

    pub(super) fn collect_at(
        &mut self,
        client: &dyn GameClient,
        target: &xenomit_behavior_engine::EntityRef,
        approach: xenomit_behavior_engine::Point,
        wait_ms: u64,
    ) -> Result<(), String> {
        let now = Instant::now();
        let id = entity_id(target)?;
        let call = FunctionCall::new(EVENT_MANAGER, "moveHeroToCordinates")
            .with_arguments(vec![
                json!(game_int(approach.x)?),
                json!(game_int(approach.y)?),
                json!({"$xenomit_entity_id": id}),
            ])
            .in_frame();
        invoke(client, call, "collect box")?;
        self.moving = true;
        self.collecting_until = Some(now + Duration::from_millis(wait_ms));
        self.last_move = Some((approach.x, approach.y, now));
        Ok(())
    }

    /// Keep the ship moving while the planner has no target in range so the
    /// module hunts across the map instead of sitting inert. The patrol
    /// sweeps the map center and quadrant centers; without map bounds it can
    /// only stop the ship. Returns true when the ship was sent moving.
    pub(super) fn roam(
        &mut self,
        client: &dyn GameClient,
        player: xenomit_behavior_engine::Point,
        snapshot: &GameSnapshot,
        settings: &Value,
        zone_targets: &[xenomit_behavior_engine::Point],
    ) -> Result<bool, String> {
        if self
            .collecting_until
            .is_some_and(|until| until > Instant::now())
        {
            return Ok(true);
        }
        let Some((width, height)) = snapshot
            .map
            .as_ref()
            .and_then(|map| map.width.zip(map.height))
        else {
            self.stop_moving(client)?;
            return Ok(false);
        };
        if width <= 0.0 || height <= 0.0 {
            self.stop_moving(client)?;
            return Ok(false);
        }
        if self
            .roam_target
            .is_none_or(|(x, y)| (player.x - x).hypot(player.y - y) < ROAM_ARRIVAL)
        {
            if !zone_targets.is_empty() {
                for _ in 0..zone_targets.len() {
                    let target = zone_targets[self.roam_index % zone_targets.len()];
                    self.roam_index = (self.roam_index + 1) % zone_targets.len();
                    self.roam_target = Some((target.x, target.y));
                    if (player.x - target.x).hypot(player.y - target.y) >= ROAM_ARRIVAL {
                        break;
                    }
                }
            } else {
                for _ in 0..ROAM_FRACTIONS.len() {
                    let (fx, fy) = ROAM_FRACTIONS[self.roam_index % ROAM_FRACTIONS.len()];
                    self.roam_index = (self.roam_index + 1) % ROAM_FRACTIONS.len();
                    let destination = (width * fx, height * fy);
                    self.roam_target = Some(destination);
                    if (player.x - destination.0).hypot(player.y - destination.1) >= ROAM_ARRIVAL {
                        break;
                    }
                }
            }
        }
        let (x, y) = self.roam_target.unwrap_or_default();
        if let Err(error) = self.routed_move(
            client,
            snapshot,
            settings,
            player,
            xenomit_behavior_engine::Point { x, y },
        ) {
            self.roam_target = None;
            self.collecting_until = None;
            self.stop_moving(client)?;
            return Err(error);
        }
        Ok(true)
    }

    pub(super) fn stop_moving(&mut self, client: &dyn GameClient) -> Result<(), String> {
        if self
            .collecting_until
            .is_some_and(|until| until > Instant::now())
        {
            return Ok(());
        }
        if self.moving {
            invoke(
                client,
                FunctionCall::new(EVENT_MANAGER, "stopHero").in_frame(),
                "stop movement",
            )?;
            self.moving = false;
            self.last_move = None;
            self.collecting_until = None;
        }
        Ok(())
    }

    pub(super) fn stop(&mut self, client: &dyn GameClient) -> Result<(), String> {
        self.router.reset();
        self.collecting_until = None;
        let movement = self.stop_moving(client);
        let target = self.sync_target(client, None, None);
        movement.and(target)
    }
}

pub(super) fn movement_call(x: f64, y: f64) -> Result<FunctionCall, String> {
    Ok(FunctionCall::new(EVENT_MANAGER, "moveHeroToCordinates")
        .with_arguments(vec![json!(game_int(x)?), json!(game_int(y)?)])
        .in_frame())
}

pub(super) fn game_int(value: f64) -> Result<i32, String> {
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        return Err(format!("movement coordinate is out of range: {value}"));
    }
    Ok(value.round() as i32)
}

pub(super) fn entity_id(target: &xenomit_behavior_engine::EntityRef) -> Result<i32, String> {
    let Some(id) = target.id.as_ref() else {
        return Err(format!("selected entity {} has no map ID", target.path));
    };
    id.as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .or_else(|| id.as_u64().and_then(|value| i32::try_from(value).ok()))
        .ok_or_else(|| format!("selected entity {} has an invalid map ID", target.path))
}

pub(super) fn invoke(client: &dyn GameClient, call: FunctionCall, action: &str) -> Result<(), String> {
    client
        .call_function(&call)
        .map(|_| ())
        .map_err(|error| format!("could not {action}: {error}"))
}
