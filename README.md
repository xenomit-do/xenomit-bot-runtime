# xenomit-bot-runtime

GUI-free bot runtime extracted from `xenomit-bot` (orchestrator phase 0 —
see `xenomit-orchestrator/docs/02-headless-bot.md` §2.1). It owns everything
the GPUI shell does not need to render, so a headless `botd` can drive the
same planner without forking the desktop app. **No `gpui`/`gpui-kit`
dependency** — keep it that way.

## Modules

| Module          | Contents |
|-----------------|----------|
| `behavior`      | `PlannerRuntime`, worker run loop, movement/collection execution, host-action queueing, Backpage service access |
| `handoff`       | graceful-handoff plumbing (`HandoffLink`, `Drain`, `RuntimeCheckpoint`, `PendingRestore`, checkpoint queue, paused active-time accounting) |
| `statistics`    | account-scoped JSONL reward journal, balance history, and the durable sequenced outbox (seq/ack cursor, replay on restart) |
| `panel`         | web-panel agent link (Agent Protocol v1) |
| `packet_client` | `xenomit-packet-client` game adapter (`ClientKind::Packet`) |
| `star_catalog`  | CDN-fetched map catalog (`resources.xml` → `maps-config`) |
| `settings`      | `BotSettings` (the `modules.json` shape), `ClientKind`, `FlashBackend` |
| `session`       | Backpage authentication |

## Front-end seams

The desktop app and `botd` inject five seams; defaults preserve today's
behavior:

| Trait                | Default                    | Purpose |
|----------------------|----------------------------|---------|
| `HostActionHandler`  | `HostActionQueue`          | route refresh/disconnect/close/profile/tag requests from rules; the queue default is drained via `take_host_actions()` (GUI polling) |
| `StatSink`           | `FileStatSink`             | append journal events — same JSONL under `%APPDATA%\XenomitBot\statistics` |
| `SettingsStore`      | `FileSettingsStore`        | `BotSettings` persistence — `%APPDATA%\XenomitBot\modules.json` |
| `SessionProvider`    | `BackpageSessionProvider`  | authenticated `Session` (password or pasted dosid) |
| `HandoffLink`        | detached                   | control-plane side of drain/restore/checkpointing — `botd` drives it via `PlannerRuntime::with_handoff_link`; unset keeps the GUI single-lifecycle |

`PlannerRuntime::with_host_action_handler` swaps the host-action route; the
other seams are injected where the front-end constructs statistics,
settings, and login.

## Handoff lifecycle (phase 3)

With a `HandoffLink` attached, the run loop answers `Drain` by checkpointing
at the next quiescent point (`CheckpointQuality::Clean`), reports `Drained`,
and exits. A `PendingRestore` posted before the next worker start restores
engine state; `capture_runtime_checkpoint` emits periodic
`CheckpointQuality::Checkpoint` writes the daemon persists
epoch-conditionally. Statistics events carry a monotonically increasing
`seq`; the journal replays `seq > cursor` on restart and `stat_ack` from the
panel side advances the cursor, so a handoff replays the unacked tail without
loss or duplication. See `xenomit-orchestrator/docs/03-graceful-handoff.md`.

## Layout contract

`xenomit-bot-gui` keeps only `view.rs` (GPUI), `live_map.rs` (GPUI),
`module_settings.rs` (GPUI), and `mod.rs` (window bootstrap). Everything
planner-adjacent lives here; the GUI consumes this crate as a git
dependency.

## Test

```sh
cargo test
```
