#[cfg(test)]
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use crate::handoff::{
    CheckpointQuality, DrainState, HandoffLink, PendingRestore, RestorePolicy, RuntimeCheckpoint,
};
use xenomit_behavior_engine::{
    ActionIntent, BehaviorDecision, BehaviorRegistry, BehaviorRunner, CancellationToken,
    CoordinateTransform, HostAction, InterruptionContext, InterruptionReason, InterruptionStatus,
    ModuleRegistry, ModuleRunner, MovementRouter, ObstacleNavigation, OperationHost, OperationPoll,
    PluginCatalog, PluginSelection, RuleContext, RunState, ServiceRegistry, ServiceRegistryBuilder,
    SkylabAction, ThreadedOperationHost, builtin_behavior_registry, filter_booty_boxes,
};
#[cfg(test)]
use xenomit_behavior_engine::{BehaviorConfig, BehaviorEngine};
#[cfg(test)]
use xenomit_game_api::EntityCategory;
use xenomit_game_api::{
    BehaviorRuntime, FunctionCall, GameClient, GameEntity, GameSnapshot, ModuleConfig,
};


mod builder;
mod executor;
mod host;
mod restore;
mod runtime;
mod services;
mod targeting;
mod worker;
#[cfg(test)]
mod tests;

use executor::*;
pub use host::*;
use restore::*;
use runtime::*;
pub use services::*;
use targeting::*;
use worker::*;

/// Default planner interval; modules can override it with a `tick_ms`
/// number in `ModuleConfig.settings` (the bot uses a slower tick when the
/// game API runs on the legacy pipe backend, which stalls the player).
const TICK: Duration = Duration::from_millis(150);

const MOVE_REFRESH: Duration = Duration::from_millis(1_500);

const MOVE_REFRESH_DISTANCE: f64 = 30.0;

/// Distance at which a roam waypoint counts as reached.
const ROAM_ARRIVAL: f64 = 400.0;

/// Patrol fractions of the map bounds that idle modules sweep in order —
/// center first because NPCs cluster there, then the quadrant centers.
const ROAM_FRACTIONS: [(f64, f64); 5] = [
    (0.5, 0.5),
    (0.25, 0.25),
    (0.75, 0.25),
    (0.75, 0.75),
    (0.25, 0.75),
];

const EVENT_MANAGER: &str =
    "net.bigpoint.darkorbit.managers.ScreenManager.getInstance().eventManager";

const SHIP_MANAGER: &str =
    "net.bigpoint.darkorbit.managers.ScreenManager.getInstance().map.getShipManager()";

const GAME_FACADE: &str = "net.bigpoint.darkorbit.mvc.application.GameFacade.getInstance()";

pub struct PlannerRuntime {
    registry: Arc<ModuleRegistry>,
    behavior_registry: Arc<BehaviorRegistry>,
    plugin_catalog: Option<Arc<PluginCatalog>>,
    plugin_diagnostics: Vec<String>,
    behavior_session: Mutex<Option<RuleSession>>,
    services: ServiceRegistry,
    host_actions: Arc<dyn HostActionHandler>,
    worker: Mutex<Option<Worker>>,
    report: Arc<Mutex<Option<String>>>,
    /// Module config pushed via `update` while the worker runs; applied on
    /// the next tick.
    pending: Arc<Mutex<Option<ModuleConfig>>>,
    /// Headless drain/checkpoint channel; `None` in the GUI.
    handoff: Option<Arc<HandoffLink>>,
}

struct Worker {
    stop: CancellationToken,
    running: Arc<AtomicBool>,
    thread: JoinHandle<Result<(), String>>,
    selection: PluginSelection,
}

struct RuleSession {
    runner: Arc<Mutex<BehaviorRunner>>,
    started: Instant,
    selection: PluginSelection,
}

impl Default for PlannerRuntime {
    fn default() -> Self {
        let mut catalog = PluginCatalog::builtin();
        let plugin_root = crate::settings::config_root().join("plugins");
        let diagnostics = catalog.load_directory(&plugin_root);
        Self::with_catalog(catalog, diagnostics, default_services())
    }
}

impl PlannerRuntime {
    /// Built-in plugins only — no user-supplied manifest directory. Managed
    /// headless daemons must not load third-party plugin manifests from disk
    /// (orchestrator `docs/09-security.md` §9.3); the GUI keeps [`Default`].
    #[must_use]
    pub fn builtin_only() -> Self {
        Self::with_catalog(PluginCatalog::builtin(), Vec::new(), default_services())
    }
}

impl Drop for PlannerRuntime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
