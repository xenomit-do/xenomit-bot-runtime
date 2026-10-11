use super::*;

/// Sink for `HostAction`s the behavior runtime cannot perform in-process.
/// The GUI polls the default [`HostActionQueue`] once a second and executes
/// each action on the UI thread; a headless front-end installs its own
/// handler (in-process re-login on `Refresh`, exit on `Close`, …) through
/// [`PlannerRuntime::with_host_action_handler`].
pub trait HostActionHandler: Send + Sync {
    fn handle(&self, action: HostAction);
}

pub(super) static HOST_ACTIONS: std::sync::OnceLock<Mutex<Vec<HostAction>>> = std::sync::OnceLock::new();

/// Default [`HostActionHandler`]: buffers actions on the process-global
/// queue drained by [`take_host_actions`]. Duplicate pending actions are
/// deduplicated, matching the queue's historical behavior.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostActionQueue;

impl HostActionHandler for HostActionQueue {
    fn handle(&self, action: HostAction) {
        if let Ok(mut actions) = HOST_ACTIONS.get_or_init(|| Mutex::new(Vec::new())).lock()
            && !actions.contains(&action)
        {
            actions.push(action);
        }
    }
}

pub fn take_host_actions() -> Vec<HostAction> {
    HOST_ACTIONS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .map(|mut actions| std::mem::take(&mut *actions))
        .unwrap_or_default()
}
