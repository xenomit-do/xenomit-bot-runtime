use super::*;

pub(super) static REFRESH_TESTS: Mutex<()> = Mutex::new(());

pub(super) fn overdue_refresh_rules() -> Arc<Mutex<BehaviorRunner>> {
    let mut rules = builtin_behavior_registry()
        .filtered(|id| id == "refresh")
        .create(&json!({"refresh":{"interval_minutes":1}}))
        .unwrap();
    rules.tick(&RuleContext {
        now_ms: 0,
        snapshot: None,
        module_settings: &Value::Null,
        stopped: false,
    });
    Arc::new(Mutex::new(rules))
}

pub(super) fn custom_descriptor(services: &[&str]) -> xenomit_game_api::ModuleDescriptor {
    xenomit_game_api::ModuleDescriptor {
        id: "custom".into(),
        name: "Custom event".into(),
        description: "Test workflow".into(),
        settings: vec![],
        required_services: services.iter().map(|s| s.to_string()).collect(),
    }
}

pub(super) fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "worker did not reach the expected state"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

use xenomit_game_api::{
    ActionResult, Bounds, CallTiming, ClientStatus, GameMapInfo, Inspection, Point as ApiPoint,
    StageInfo,
};

pub(super) struct FakeClient {
    pub(super) ready: bool,
    pub(super) snapshot: Mutex<GameSnapshot>,
    pub(super) calls: Mutex<Vec<FunctionCall>>,
    pub(super) fail_collection: AtomicBool,
    pub(super) commands: Mutex<Vec<xenomit_game_api::GameCommand>>,
}

impl FakeClient {
    pub(super) fn new(snapshot: GameSnapshot) -> Self {
        Self {
            ready: true,
            snapshot: Mutex::new(snapshot),
            calls: Mutex::new(Vec::new()),
            fail_collection: AtomicBool::new(false),
            commands: Mutex::new(vec![]),
        }
    }

    pub(super) fn functions(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.function.clone())
            .collect()
    }

    pub(super) fn calls(&self, function: &str) -> Vec<FunctionCall> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.function == function)
            .cloned()
            .collect()
    }
}

impl GameClient for FakeClient {
    fn game_command(
        &self,
        command: &xenomit_game_api::GameCommand,
    ) -> xenomit_game_api::Result<ActionResult> {
        self.commands.lock().unwrap().push(command.clone());
        if matches!(command, xenomit_game_api::GameCommand::JumpPortal { .. }) {
            let mut snapshot = self.snapshot.lock().unwrap();
            snapshot.map.as_mut().unwrap().id = Some(2);
            snapshot.navigation.map_id = Some(2);
        }
        Ok(ActionResult::default())
    }
    fn status(&self) -> xenomit_game_api::Result<ClientStatus> {
        Ok(ClientStatus {
            backend: "fake".into(),
            connected: true,
            ready: self.ready,
            details: BTreeMap::new(),
        })
    }

    fn ping(&self) -> xenomit_game_api::Result<()> {
        Ok(())
    }

    fn snapshot(&self, _target: Option<&str>) -> xenomit_game_api::Result<GameSnapshot> {
        Ok(self.snapshot.lock().unwrap().clone())
    }

    fn inspect(&self, _target: &str) -> xenomit_game_api::Result<Inspection> {
        Ok(Inspection::default())
    }

    fn call_function(&self, call: &FunctionCall) -> xenomit_game_api::Result<ActionResult> {
        self.calls.lock().unwrap().push(call.clone());
        if call.function == "moveHeroToCordinates"
            && call.arguments.len() == 3
            && self.fail_collection.load(Ordering::Acquire)
        {
            return Err(xenomit_game_api::ApiError::Transport(
                "collection failed".into(),
            ));
        }
        Ok(ActionResult::default())
    }
}

pub(super) fn module(id: &str) -> ModuleConfig {
    ModuleConfig {
        module_id: id.into(),
        settings: json!({}),
    }
}

pub(super) fn entity(category: EntityCategory, name: &str, x: f64, y: f64, id: i64) -> GameEntity {
    GameEntity {
        path: format!("stage.{name}"),
        class_name: name.into(),
        name: name.into(),
        category,
        position: ApiPoint { x, y },
        bounds: Bounds::default(),
        id: Some(json!(id)),
        health: None,
        max_health: None,
        shield: None,
        max_shield: None,
        fields: Default::default(),
    }
}

pub(super) fn snapshot(entities: Vec<GameEntity>) -> GameSnapshot {
    GameSnapshot {
        navigation: Default::default(),
        observed_at_unix_ms: 1,
        stage: StageInfo::default(),
        map: Some(GameMapInfo {
            id: Some(1),
            name: Some("1-8".into()),
            width: Some(21_000.0),
            height: Some(13_100.0),
            player_position: None,
            safe_places: Vec::new(),
            in_safe_place: None,
        }),
        entities,
        visited: 1,
        truncated: false,
    }
}

pub(super) fn npc_killer(client: &FakeClient) -> (BehaviorEngine, ActionExecutor) {
    let planner =
        BehaviorEngine::new(configured_behavior(&module("npc_killer")).unwrap()).unwrap();
    let _ = client;
    (planner, ActionExecutor::default())
}

pub(super) fn collecting_client(with_npc: bool) -> FakeClient {
    let mut box_entity = entity(EntityCategory::Unknown, "box", 200.0, 100.0, 3);
    box_entity.fields.insert("entityKind".into(), json!("box"));
    let mut entities = vec![
        entity(EntityCategory::Player, "hero", 100.0, 100.0, 1),
        box_entity,
    ];
    if with_npc {
        entities.push(entity(EntityCategory::Npc, "streuner", 700.0, 100.0, 2));
    }
    FakeClient::new(snapshot(entities))
}

mod executor;
mod runtime;
