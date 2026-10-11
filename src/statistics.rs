//! Account-scoped reward journal and authoritative balance history.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use xenomit_backpage::{Session, client::BackpageClient};

use crate::panel::PanelLink;
use crate::settings::config_root;

pub const METRICS: [&str; 4] = ["Credits", "Uridium", "Honor", "Experience"];
pub type SharedStats = Arc<Mutex<Statistics>>;
const RETENTION: u64 = 30 * 24 * 3_600_000;
const MAX_EVENTS: usize = 100_000;

/// `config_root()/statistics` — the default journal directory the GUI uses.
pub fn default_directory() -> PathBuf {
    config_root().join("statistics")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Events are event-sourced (docs/03 §3.6): each gets a monotonic `seq` and
/// stays in the outbox until the control plane acknowledges it. Older
/// journals written before seq existed deserialize as 0.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub seq: u64,
    pub at: u64,
    pub source: String,
    pub message: Option<String>,
    pub rewards: [f64; 4],
    pub balances: [Option<f64>; 4],
}

/// Upper bound of unacked events retained for the snapshot outbox tail
/// (docs/03 §3.6 `CHECKPOINT_OUTBOX_TAIL` = 1000).
pub const OUTBOX_TAIL: usize = 1000;

/// Where appended journal events are persisted or forwarded. The GUI writes
/// JSONL journals through [`FileStatSink`]; a headless front-end can forward
/// events to a control-plane outbox instead (or as well) by installing its
/// own sink on [`Statistics`].
pub trait StatSink: Send + Sync {
    /// Persist or forward one appended event. An `Err` message is surfaced
    /// on `Statistics::error`.
    fn record(&self, event: &Event) -> Result<(), String>;
    /// Persist the acknowledged outbox cursor (default: no persistence).
    fn ack(&self, _cursor: u64) -> Result<(), String> {
        Ok(())
    }
    /// Load a previously persisted cursor, if the sink stores one.
    fn load_cursor(&self) -> Option<u64> {
        None
    }
}

/// The default [`StatSink`]: one serialized [`Event`] per line at `path`.
pub struct FileStatSink {
    path: PathBuf,
}

impl FileStatSink {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl StatSink for FileStatSink {
    fn record(&self, event: &Event) -> Result<(), String> {
        (|| -> std::io::Result<()> {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            serde_json::to_writer(&mut file, event)?;
            writeln!(file)?;
            Ok(())
        })()
        .map_err(|error| format!("Could not save statistics: {error}"))
    }

    /// Persist the cursor to a `<journal>.cursor` sidecar, atomically.
    fn ack(&self, cursor: u64) -> Result<(), String> {
        (|| -> std::io::Result<()> {
            let sidecar = PathBuf::from(format!("{}.cursor", self.path.display()));
            let tmp = PathBuf::from(format!("{}.tmp", sidecar.display()));
            std::fs::write(&tmp, cursor.to_string())?;
            std::fs::rename(&tmp, &sidecar)?;
            Ok(())
        })()
        .map_err(|error| format!("Could not save statistics cursor: {error}"))
    }

    fn load_cursor(&self) -> Option<u64> {
        std::fs::read_to_string(PathBuf::from(format!("{}.cursor", self.path.display())))
            .ok()
            .and_then(|body| body.trim().parse::<u64>().ok())
    }
}

pub struct Statistics {
    pub events: VecDeque<Event>,
    pub balances: [Option<f64>; 4],
    pub started: u64,
    pub ended: Option<u64>,
    pub last_reconciled: Option<u64>,
    pub error: Option<String>,
    pub backpage_error: Option<String>,
    pub account: String,
    /// Set while a session is attached to a paired web panel; appended
    /// events stream out as `stat_event` messages.
    pub panel: Option<Arc<PanelLink>>,
    /// Events not yet acknowledged by the control plane (docs/03 §3.6).
    /// Bounded to [`OUTBOX_TAIL`]; the oldest unacked entry is dropped first.
    outbox: VecDeque<Event>,
    /// Last `seq` acknowledged upstream; persisted through the sink.
    cursor: u64,
    /// Next `seq` to assign.
    next_seq: u64,
    /// True once an event was dropped from the full outbox.
    pub outbox_dropped: bool,
    user_id: i64,
    sinks: Vec<Arc<dyn StatSink>>,
}

impl Statistics {
    /// Load the journal under `directory` and record new events through a
    /// [`FileStatSink`] — the GUI's historical behavior.
    pub fn load(session: &Session, directory: PathBuf) -> SharedStats {
        Self::load_with_sinks(session, directory, |path| {
            vec![Arc::new(FileStatSink::new(path))]
        })
    }

    /// Same journal preload as [`Statistics::load`], with the sink set
    /// chosen by `sinks`: a headless front-end can skip the file entirely or
    /// fan out to an outbox alongside it.
    pub fn load_with_sinks(
        session: &Session,
        directory: PathBuf,
        sinks: impl FnOnce(PathBuf) -> Vec<Arc<dyn StatSink>>,
    ) -> SharedStats {
        let server: String = session
            .server
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = directory.join(format!("{server}-{}.jsonl", session.user_id));
        let sinks = sinks(path.clone());
        let cursor = sinks.iter().find_map(|sink| sink.load_cursor()).unwrap_or(0);
        let mut stats = Self {
            events: VecDeque::new(),
            balances: [None; 4],
            started: now_ms(),
            ended: None,
            last_reconciled: None,
            error: None,
            backpage_error: None,
            account: format!("{} / {}", session.server, session.user_id),
            panel: None,
            outbox: VecDeque::new(),
            cursor,
            next_seq: cursor,
            outbox_dropped: false,
            user_id: session.user_id,
            sinks,
        };
        match std::fs::File::open(&path) {
            Ok(file) => {
                for line in BufReader::new(file).lines() {
                    match line
                        .ok()
                        .and_then(|s| serde_json::from_str::<Event>(&s).ok())
                    {
                        Some(event) => {
                            stats.balances = event.balances;
                            stats.next_seq = stats.next_seq.max(event.seq);
                            if event.seq > stats.cursor
                                && stats.outbox.len() < OUTBOX_TAIL
                            {
                                stats.outbox.push_back(event.clone());
                            }
                            if event.message.is_none() {
                                stats.last_reconciled = Some(event.at);
                            }
                            if event.at >= stats.started.saturating_sub(RETENTION) {
                                stats.events.push_back(event);
                                if stats.events.len() > MAX_EVENTS {
                                    stats.events.pop_front();
                                }
                            }
                        }
                        None => {
                            stats.error =
                                Some("Some saved statistics records could not be read".into())
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => stats.error = Some(format!("Could not load statistics: {e}")),
        }
        Arc::new(Mutex::new(stats))
    }

    /// Add a sink that records every subsequently appended event (a panel
    /// outbox, a test recorder, …).
    pub fn add_sink(&mut self, sink: Arc<dyn StatSink>) {
        self.sinks.push(sink);
    }

    fn append(&mut self, mut event: Event) {
        self.next_seq += 1;
        event.seq = self.next_seq;
        self.outbox.push_back(event.clone());
        while self.outbox.len() > OUTBOX_TAIL {
            self.outbox.pop_front();
            self.outbox_dropped = true;
        }
        for sink in &self.sinks {
            if let Err(message) = sink.record(&event) {
                self.error = Some(message);
            }
        }
        if let Some(link) = &self.panel {
            link.push_stat_event(
                event.seq,
                event.at,
                &event.source,
                event.message.as_deref(),
                event.rewards,
                event.balances,
            );
        }
        let cutoff = event.at.saturating_sub(RETENTION);
        self.events.push_back(event);
        while self.events.len() > MAX_EVENTS || self.events.front().is_some_and(|e| e.at < cutoff) {
            self.events.pop_front();
        }
    }

    /// Last `seq` the control plane acknowledged.
    pub fn outbox_cursor(&self) -> u64 {
        self.cursor
    }
    /// Highest `seq` assigned so far.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }
    /// Unacked events, oldest first, bounded to [`OUTBOX_TAIL`].
    pub fn outbox_tail(&self) -> impl Iterator<Item = &Event> {
        self.outbox.iter()
    }
    /// Count of events still waiting for acknowledgement.
    pub fn unacked_len(&self) -> usize {
        self.outbox.len()
    }

    /// Acknowledge `cursor` upstream: drop covered events and persist the
    /// cursor through sinks that support it. Monotonic — an older cursor is
    /// a no-op.
    pub fn ack(&mut self, cursor: u64) {
        if cursor <= self.cursor {
            return;
        }
        self.cursor = cursor;
        while self.outbox.front().is_some_and(|event| event.seq <= cursor) {
            self.outbox.pop_front();
        }
        for sink in &self.sinks {
            if let Err(message) = sink.ack(cursor) {
                self.error = Some(message);
            }
        }
    }

    /// Seed the outbox from a restored snapshot tail (docs/03 §3.6). `seq`
    /// numbering continues past the highest restored event and cursor.
    /// Events already replayed from the journal are skipped — a restart on
    /// the same `state_dir` would otherwise see them twice.
    pub fn restore_outbox(&mut self, cursor: u64, tail: Vec<Event>) {
        self.cursor = self.cursor.max(cursor);
        for event in tail {
            if event.seq > self.cursor
                && !self.outbox.iter().any(|queued| queued.seq == event.seq)
            {
                self.next_seq = self.next_seq.max(event.seq);
                if self.outbox.len() < OUTBOX_TAIL {
                    self.outbox.push_back(event);
                }
            }
        }
        // Journal replay + tail merge can interleave seq order; keep the
        // outbox sorted so `ack` drops a strict prefix.
        if self.outbox.len() > 1 {
            self.outbox.make_contiguous().sort_by_key(|e| e.seq);
        }
    }

    pub fn log(&mut self, message: &str, source: &str, at: u64) {
        let rewards = parse_rewards(message);
        self.log_with_rewards(message, source, at, rewards);
    }

    fn log_with_rewards(&mut self, message: &str, source: &str, at: u64, rewards: [f64; 4]) {
        for (index, value) in rewards.iter().enumerate() {
            if let Some(balance) = &mut self.balances[index] {
                *balance += value;
            }
        }
        if let Some((index, _, total)) = xenomit_packet_client::model::asset_log(message) {
            self.balances[index] = Some(total);
            self.last_reconciled = Some(at);
        }
        self.append(Event {
            seq: 0,
            at,
            source: source.into(),
            message: Some(message.into()),
            rewards,
            balances: self.balances,
        });
    }

    pub fn reconcile(&mut self, balances: [Option<f64>; 4], source: &str, at: u64) {
        let mut updated = false;
        for (index, value) in balances.into_iter().enumerate() {
            if let Some(value) = value.filter(|v| v.is_finite()) {
                self.balances[index] = Some(value);
                updated = true;
            }
        }
        if updated {
            self.last_reconciled = Some(at);
            self.append(Event {
                seq: 0,
                at,
                source: source.into(),
                message: None,
                rewards: [0.0; 4],
                balances: self.balances,
            });
        }
    }

    pub fn client_line(&mut self, line: &str) {
        let (prefix, reward) = if line.starts_with("[game-log] ") {
            ("[game-log] ", true)
        } else if line.starts_with("[game-assets] ") {
            ("[game-assets] ", false)
        } else {
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line[prefix.len()..]) else {
            return;
        };
        if value["user_id"].as_i64() != Some(self.user_id) {
            return;
        }
        if reward {
            if let Some(message) = value["message"].as_str() {
                let rewards = serde_json::from_value::<[f64; 4]>(value["rewards"].clone())
                    .ok()
                    .filter(|values| values.iter().all(|v| v.is_finite() && *v >= 0.0))
                    .unwrap_or_else(|| parse_rewards(message));
                self.log_with_rewards(message, "game", now_ms(), rewards);
            }
        } else {
            self.reconcile(
                ["credits", "uridium", "honor", "experience"]
                    .map(|key| value["assets"][key].as_f64()),
                "game",
                now_ms(),
            );
        }
    }

    /// Gross rewards in the last hour of this connection. Corrections and
    /// spending never become earnings; the denominator is observed time.
    pub fn rates(&self, at: u64) -> [f64; 4] {
        let at = self.ended.map_or(at, |ended| at.min(ended));
        let since = self.started.max(at.saturating_sub(3_600_000));
        let mut earned = [0.0; 4];
        for event in self.events.iter().filter(|e| e.at >= since && e.at <= at) {
            for (index, value) in event.rewards.iter().enumerate() {
                earned[index] += value;
            }
        }
        let hours = at.saturating_sub(since) as f64 / 3_600_000.0;
        if hours > 0.0 {
            earned.map(|v| v / hours)
        } else {
            [0.0; 4]
        }
    }
}

/// Only explicit reward sentences are interpreted. Unknown/localized logs
/// are still retained, and periodic totals keep the asset history correct.
fn parse_rewards(message: &str) -> [f64; 4] {
    if let Some((index, delta, _)) = xenomit_packet_client::model::asset_log(message) {
        let mut rewards = [0.0; 4];
        rewards[index] = delta.max(0.0);
        return rewards;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let lower = message.to_lowercase();
    if !["received", "earned", "obtained", "awarded"]
        .iter()
        .any(|verb| lower.contains(verb))
        || ["spent", "cost", "lost", "deducted"]
            .iter()
            .any(|verb| lower.contains(verb))
    {
        return [0.0; 4];
    }
    let pattern = RE.get_or_init(|| Regex::new(r"(?i)([0-9]+(?:[.,'\x{a0} ][0-9]{3})*)\s*(credits?\b|uridium\b|honou?r(?: points?)?\b|experience(?: points?)?\b|EP\b)").unwrap());
    let mut values = [0.0; 4];
    for capture in pattern.captures_iter(message) {
        if capture
            .get(1)
            .is_some_and(|m| message[..m.start()].ends_with('-'))
        {
            continue;
        }
        let amount: String = capture[1].chars().filter(char::is_ascii_digit).collect();
        if let Ok(amount) = amount.parse::<f64>() {
            let unit = capture[2].to_ascii_lowercase();
            let index = if unit.starts_with("credit") {
                0
            } else if unit == "uridium" {
                1
            } else if unit.starts_with("hon") {
                2
            } else {
                3
            };
            values[index] += amount;
        }
    }
    values
}

pub fn fetch_balances(session: Session) -> Result<[Option<f64>; 4], String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let client = BackpageClient::new(session).map_err(|e| e.to_string())?;
        let response = client.hangar_list().await.map_err(|e| e.to_string())?;
        if response.is_error != 0 {
            return Err("Backpage rejected the balance request".into());
        }
        let money = response
            .data
            .and_then(|data| data.money)
            .ok_or("Backpage response has no balances")?;
        let parse = |value: Option<String>| {
            value
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| v.is_finite())
        };
        let balances = [parse(money.credits), parse(money.uridium), None, None];
        if balances[..2].iter().all(Option::is_none) {
            return Err("Backpage returned invalid balances".into());
        }
        Ok(balances)
    })
}

#[cfg(test)]
mod tests;
