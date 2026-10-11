//! GUI-free bot runtime extracted from `xenomit-bot` (orchestrator phase 0,
//! see `xenomit-orchestrator/docs/02-headless-bot.md` §2.1).
//!
//! Owns everything the GPUI shell does not need to render:
//!
//! * [`behavior`] — `PlannerRuntime`, the worker run loop, movement/collection
//!   execution, host-action queueing and Backpage service access.
//! * [`statistics`] — the account-scoped reward journal and balance history.
//! * [`panel`] — the web-panel agent link (Agent Protocol v1).
//! * [`packet_client`] — the `xenomit-packet-client` game adapter.
//! * [`star_catalog`] — the CDN-fetched map catalog.
//! * [`settings`] — `BotSettings` (the modules.json shape) and persistence.
//! * [`session`] — Backpage authentication.
//!
//! Front-ends (the GPUI app today, `botd` later) inject four seams:
//!
//! * [`HostActionHandler`] — what happens when a rule asks the host to
//!   refresh/disconnect/close/switch profile. The default
//!   [`HostActionQueue`] buffers on the process-global queue drained by
//!   [`take_host_actions`], which is how the GUI stays poll-driven.
//! * [`StatSink`] — where appended journal events go. [`FileStatSink`]
//!   writes the same JSONL the GUI always wrote.
//! * [`SettingsStore`] — `BotSettings` persistence. [`FileSettingsStore`]
//!   is `%APPDATA%\XenomitBot\modules.json`.
//! * [`SessionProvider`] — where the authenticated `Session` comes from.
//!   [`BackpageSessionProvider`] covers both sign-in flows the GUI form
//!   offers (password or pasted dosid).

pub mod behavior;
pub mod handoff;
pub mod packet_client;
pub mod panel;
pub mod session;
pub mod settings;
pub mod star_catalog;
pub mod statistics;

pub use behavior::{
    HostActionHandler, HostActionQueue, PlannerRuntime, set_backpage_session, take_host_actions,
};
pub use handoff::{
    CheckpointQuality, Drain, DrainState, Drained, HandoffLink, PendingRestore, RestorePolicy,
    RuntimeCheckpoint,
};
pub use session::{BackpageSessionProvider, SessionCredentials, SessionProvider};
pub use settings::{BotSettings, ClientKind, FileSettingsStore, FlashBackend, SettingsStore};
pub use statistics::{FileStatSink, SharedStats, StatSink, Statistics};
