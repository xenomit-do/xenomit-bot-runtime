//! Cooperative drain/checkpoint plumbing (orchestrator phase 3, see
//! `xenomit-orchestrator/docs/03-graceful-handoff.md`).
//!
//! The run loop never blocks on the control plane: a `Drain` directive is
//! posted onto the shared [`HandoffLink`], evaluated every tick through the
//! same permission ladder as `ClientRefresh`, and answered with [`Drained`]
//! once the worker has stopped movement/targeting and captured state.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use xenomit_behavior_engine::ModuleState;

/// Fidelity of a captured [`RuntimeCheckpoint`]. Mirrors the snapshot
/// `quality` field (docs/03 §3.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckpointQuality {
    /// Captured at a safe point after a cooperative drain.
    #[default]
    Clean,
    /// Captured after the drain deadline at a quiescent tick; in-flight
    /// work was discarded and recorded under `issues`.
    Degraded,
    /// Periodic crash-path checkpoint; may be captured mid-work.
    Checkpoint,
}

/// A drain directive delivered by the daemon's control plane.
#[derive(Clone, Debug)]
pub struct Drain {
    /// Human/log reason (upgrade, eviction, desired-state stop, ...).
    pub reason: String,
    /// Cooperative window. While the module reports `non_interruptible`
    /// this countdown pauses; the wall clock keeps running.
    pub drain_deadline: Duration,
    /// Hard cutoff: past it the worker snapshots whatever is stable and
    /// exits, regardless of interruption permission.
    pub hard_deadline: Duration,
    /// W3C trace context to propagate into the drained report.
    pub traceparent: Option<String>,
}

/// The run loop's answer to a [`Drain`]: state was captured at the given
/// quality and the worker is done moving/attacking.
#[derive(Clone, Debug)]
pub struct Drained {
    pub quality: CheckpointQuality,
    pub checkpoint: RuntimeCheckpoint,
    /// Deferred reasons seen while waiting for the safe point.
    pub wait_reasons: Vec<String>,
    /// Why the drain concluded (safe point, drain deadline, hard deadline).
    pub concluded: &'static str,
}

/// Runtime-owned slice of a bot snapshot: the objective module's retained
/// state, every general rule's timers, and the run clock. Written under
/// `module.state`/`behaviors`/`runtime` when the daemon assembles a
/// `BotSnapshot`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeCheckpoint {
    pub schema: u32,
    /// Active objective module id (`None` while stopped/idle).
    pub module_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<ModuleState>,
    /// `rule_id -> rule blob` from `BehaviorRunner::export_state`.
    #[serde(default)]
    pub rules: Value,
    /// Milliseconds on the module clock at export. Restored workers shift
    /// their clock by this so rebased durations stay consistent.
    #[serde(default)]
    pub run_elapsed_ms: u64,
    /// Last observed world position — `{"map_id","hero_x","hero_y"}` —
    /// validation-only (docs/03 §3.3 `game_hint`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_hint: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<String>,
}
impl RuntimeCheckpoint {
    pub const SCHEMA: u32 = 1;
}

/// A checkpoint handed to a successor before its worker starts.
#[derive(Clone, Debug)]
pub struct PendingRestore {
    /// Snapshot id this restore came from (for logging/status).
    pub snapshot_id: Option<String>,
    pub checkpoint: RuntimeCheckpoint,
    pub quality: CheckpointQuality,
    /// Trace id propagated from the drain that produced the snapshot.
    pub trace_id: Option<String>,
    /// How a restore failure is treated (strict fails activation).
    pub policy: RestorePolicy,
}

/// How the runtime should treat a failed module restore on a clean
/// checkpoint (docs/03 §3.5; the daemon maps this from config).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestorePolicy {
    /// Incompatible checkpoint: fail activation loudly.
    Strict,
    /// Log the mismatch and start the module fresh.
    #[default]
    BestEffort,
}

/// Shared drain/restore/checkpoint channel between the daemon's control
/// thread and the behavior worker. All methods are non-blocking.
#[derive(Default)]
pub struct HandoffLink {
    drain: Mutex<Option<Drain>>,
    restore: Mutex<Option<PendingRestore>>,
    restore_result: Mutex<Option<Result<(), String>>>,
    drained: Mutex<Option<Drained>>,
    checkpoints: Mutex<VecDeque<RuntimeCheckpoint>>,
    last_checkpoint: Mutex<Option<Instant>>,
    /// How often the run loop captures crash-path checkpoints. `None`
    /// disables periodic capture.
    interval: Mutex<Option<Duration>>,
}
impl HandoffLink {
    pub fn new(checkpoint_interval: Option<Duration>) -> Self {
        Self {
            interval: Mutex::new(checkpoint_interval),
            ..Self::default()
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Post a drain directive. Returns the existing directive if one is
    /// already active — drains are not re-entrant.
    pub fn post_drain(&self, directive: Drain) -> Result<(), Drain> {
        let mut slot = Self::lock(&self.drain);
        if slot.is_some() {
            return Err(directive);
        }
        *slot = Some(directive);
        Ok(())
    }
    /// The active drain directive, if any. Stays posted until [`Self::post_drained`].
    pub fn drain(&self) -> Option<Drain> {
        Self::lock(&self.drain).clone()
    }
    /// Post the restore a just-started worker should apply.
    pub fn post_restore(&self, restore: PendingRestore) {
        *Self::lock(&self.restore) = Some(restore);
    }
    /// Take the pending restore; the run loop calls this once at start.
    pub fn take_restore(&self) -> Option<PendingRestore> {
        Self::lock(&self.restore).take()
    }
    /// Worker-side: publish whether the pending restore applied. The daemon
    /// reads this to distinguish a strict-policy failure (exit 70) from a
    /// best-effort fallback.
    pub fn post_restore_result(&self, result: Result<(), String>) {
        *Self::lock(&self.restore_result) = Some(result);
    }
    /// Control-side: take the worker's restore outcome, if reported yet.
    pub fn take_restore_result(&self) -> Option<Result<(), String>> {
        Self::lock(&self.restore_result).take()
    }
    /// Worker-side: publish the drain outcome and clear the directive.
    pub fn post_drained(&self, drained: Drained) {
        *Self::lock(&self.drained) = Some(drained);
        *Self::lock(&self.drain) = None;
    }
    /// Control-side: take the worker's drain outcome.
    pub fn take_drained(&self) -> Option<Drained> {
        Self::lock(&self.drained).take()
    }

    /// True when a periodic checkpoint is due (or overdue).
    pub fn checkpoint_due(&self) -> bool {
        let Some(interval) = *Self::lock(&self.interval) else {
            return false;
        };
        Self::lock(&self.last_checkpoint).is_none_or(|at| at.elapsed() >= interval)
    }
    /// Worker-side: publish a captured checkpoint.
    pub fn post_checkpoint(&self, checkpoint: RuntimeCheckpoint) {
        // Keep at most a couple pending — the daemon drains each tick; a
        // stuck daemon must not let these accumulate unboundedly.
        const MAX_PENDING: usize = 2;
        let mut queue = Self::lock(&self.checkpoints);
        while queue.len() >= MAX_PENDING {
            queue.pop_front();
        }
        queue.push_back(checkpoint);
        *Self::lock(&self.last_checkpoint) = Some(Instant::now());
    }
    /// Control-side: take the newest pending checkpoint, if any.
    pub fn take_checkpoint(&self) -> Option<RuntimeCheckpoint> {
        Self::lock(&self.checkpoints).pop_front()
    }
    /// Update the periodic-capture interval (config reload).
    pub fn set_interval(&self, interval: Option<Duration>) {
        *Self::lock(&self.interval) = interval;
    }
}

/// Worker-side drain bookkeeping: the drain deadline counts active time —
/// paused while the module reports `non_interruptible` — while the hard
/// deadline counts wall time from when the directive was received.
#[derive(Debug)]
pub struct DrainState {
    pub directive: Drain,
    started: Instant,
    paused_total: Duration,
    paused_since: Option<Instant>,
    /// Deferred reasons observed, for the drained report (deduplicated).
    pub wait_reasons: Vec<String>,
}
impl DrainState {
    pub fn new(directive: Drain) -> Self {
        Self {
            directive,
            started: Instant::now(),
            paused_total: Duration::ZERO,
            paused_since: None,
            wait_reasons: Vec::new(),
        }
    }
    /// Recompute the pause flag for this tick.
    pub fn tick(&mut self, non_interruptible: bool) {
        match (non_interruptible, self.paused_since) {
            (true, None) => self.paused_since = Some(Instant::now()),
            (false, Some(since)) => {
                self.paused_total += since.elapsed();
                self.paused_since = None;
            }
            _ => {}
        }
    }
    /// Time counting against `drain_deadline` (excludes paused spans).
    pub fn active_elapsed(&self) -> Duration {
        self.started
            .elapsed()
            .saturating_sub(self.paused_total)
            .saturating_sub(self.paused_since.map(|t| t.elapsed()).unwrap_or_default())
    }
    /// Wall time since the directive was received (hard deadline).
    pub fn wall_elapsed(&self) -> Duration {
        self.started.elapsed()
    }
    pub fn drain_deadline_passed(&self) -> bool {
        self.active_elapsed() >= self.directive.drain_deadline
    }
    pub fn hard_deadline_passed(&self) -> bool {
        self.wall_elapsed() >= self.directive.hard_deadline
    }
    pub fn note_deferred(&mut self, reason: &str) {
        if self.wait_reasons.last().map(String::as_str) != Some(reason) {
            self.wait_reasons.push(reason.to_owned());
        }
    }
}
