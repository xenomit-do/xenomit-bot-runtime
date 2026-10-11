//! Outbound connector to a Xenomit web panel (Agent Protocol v1).
//!
//! The link is split in two halves that never block each other:
//!
//! * [`PanelLink`] — shared state owned by the GPUI view. The view pushes the
//!   latest `LiveFrame`, a serialized status object, module descriptors and
//!   stat events; the agent thread drains them at its own rate. Panel commands
//!   travel the other way on the same struct.
//! * [`agent_main`] — a daemon thread that owns a blocking tungstenite socket.
//!   It reconnects with backoff, sends `hello`, streams `frame`/`status`,
//!   flushes `stat_event`/`log`/`config`/`command_result` messages, and parses
//!   `command`/`set_stream_rate`/`ping`.
//!
//! Nothing in this module touches `Session.sid`/cookies: the panel protocol
//! carries no session credentials by design.
use std::collections::VecDeque;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tungstenite::client::IntoClientRequest;
use xenomit_game_api::live::{LiveEntity, LiveFrame, LiveKind};
use xenomit_game_api::{ObstacleShape, Point};

/// Deferred command reply: the link plus the command's wire id. Passed into
/// async start/stop flows so `command_result` is emitted on completion.
pub type CommandReply = (Arc<PanelLink>, String);

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: u32 = 1;
const READ_TIMEOUT: Duration = Duration::from_millis(150);
const KEEPALIVE: Duration = Duration::from_secs(25);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Frames are rebuilt at most at 12 Hz; the wire rate defaults to 4 Hz and is
/// server-adjustable, so a cheap local cap keeps 60 Hz captures cheap.
const FRAME_PUSH_MIN: Duration = Duration::from_millis(80);
const MAX_OUTBOX: usize = 4096;
const MAX_LINE: usize = 4000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Persisted panel pairing state, stored inside `BotSettings` (modules.json).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelSettings {
    /// Master switch; unlink clears `token` instead.
    pub enabled: bool,
    /// Panel base URL, e.g. `https://dash.example.com` (ws/wss also accepted).
    pub url: String,
    /// Permanent instance token from the pairing exchange. Never logged.
    pub token: String,
    pub instance_id: String,
    /// Last known identity so `hello` is stable before login/after restart.
    pub server: String,
    pub user_id: String,
    pub username: String,
}

/// A `command` message queued for the UI side of the link.
#[derive(Clone, Debug)]
pub struct PanelCommand {
    pub id: String,
    pub action: String,
    pub payload: Value,
}

struct Inner {
    settings: PanelSettings,
    /// Bumped whenever `configure` stores new settings; the agent reconnects
    /// when the revision under the socket no longer matches.
    revision: u64,
    // Producer slots — the newest value wins; the agent never waits on the UI.
    hello: Option<Value>,
    frame: Option<Value>,
    status: Option<Value>,
    /// Star catalog last queued, compared by a cheap signature.
    catalog_sig: (usize, usize, i32, i32),
    /// Coalesced `config` slot: the newest body wins and the agent sends at
    /// most one `config` message per change, so per-frame settings writes
    /// (e.g. NPC species discovery) cannot flood the socket.
    config_next: Option<Value>,
    /// Last `config` body handed to the socket.
    config_sent: Option<Value>,
    outbox: VecDeque<Value>,
    /// `stat_ack` cursors received from the panel, for `Statistics::ack`.
    stat_acks: VecDeque<u64>,
    commands: VecDeque<PanelCommand>,
    // Shared client-log buffer for the current session plus a read cursor.
    logs: Option<Arc<Mutex<VecDeque<String>>>>,
    log_cursor: usize,
    last_frame_push: Instant,
    // UI-facing state.
    phase: &'static str,
    detail: String,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            settings: PanelSettings::default(),
            revision: 0,
            hello: None,
            frame: None,
            status: None,
            catalog_sig: (0, 0, 0, 0),
            config_next: None,
            config_sent: None,
            outbox: VecDeque::new(),
            stat_acks: VecDeque::new(),
            commands: VecDeque::new(),
            logs: None,
            log_cursor: 0,
            last_frame_push: Instant::now() - Duration::from_secs(60),
            phase: "disabled",
            detail: String::new(),
        }
    }
}

/// Shared endpoint used by `BotView` (producer/command executor) and the
/// agent thread (consumer/socket owner).
pub struct PanelLink {
    inner: Mutex<Inner>,
    online: AtomicBool,
    shutdown: AtomicBool,
}

impl Default for PanelLink {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            online: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        }
    }
}

impl PanelLink {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Replace the pairing configuration. Any revision change forces the
    /// agent to reconnect with the new token/URL.
    pub fn configure(&self, settings: PanelSettings) {
        let mut inner = self.lock();
        inner.settings = settings;
        inner.revision += 1;
    }

    /// `hello` body minus `type`/`protocol`, refreshed by the UI. The agent
    /// clones it on every (re)connect, so per-tick updates are fine.
    pub fn set_hello(&self, hello: Value) {
        self.lock().hello = Some(hello);
    }

    /// Latest frame pushed by the live monitor. Throttled to ~12 Hz; the
    /// expensive JSON build happens in the caller only when the agent is
    /// online and due.
    pub fn wants_frame(&self) -> bool {
        self.online.load(Ordering::Relaxed) && {
            let mut inner = self.lock();
            if inner.last_frame_push.elapsed() >= FRAME_PUSH_MIN {
                inner.last_frame_push = Instant::now();
                true
            } else {
                false
            }
        }
    }

    pub fn push_frame(&self, frame: &LiveFrame) {
        let mut inner = self.lock();
        inner.frame = Some(frame_payload(frame));
        let system = &frame.navigation.star_system;
        let sig = (
            system.maps.len(),
            system.links.len(),
            system.maps.first().map(|m| m.id).unwrap_or(0),
            system.maps.last().map(|m| m.id).unwrap_or(0),
        );
        if !system.maps.is_empty() && sig != inner.catalog_sig {
            inner.catalog_sig = sig;
            inner.outbox.push_back(json!({
                "type": "catalog",
                "maps": system.maps.iter().map(|m| json!({
                    "id": m.id, "name": m.name, "min_level": m.min_level,
                })).collect::<Vec<_>>(),
                "links": system.links.iter().map(|l| json!({
                    "from": l.from, "to": l.to, "portal_id": l.portal_id,
                })).collect::<Vec<_>>(),
            }));
            if inner.outbox.len() > MAX_OUTBOX {
                inner.outbox.pop_front();
            }
        }
    }

    /// Latest status body (without `type`), refreshed by the status monitor.
    pub fn set_status(&self, status: Value) {
        self.lock().status = Some(status);
    }

    /// Queue a `stat_event` — called from `Statistics::append`, which runs on
    /// whatever thread recorded the reward. `seq` is the event's outbox
    /// sequence so the panel (or control plane) can acknowledge it.
    pub fn push_stat_event(
        &self,
        seq: u64,
        at: u64,
        source: &str,
        message: Option<&str>,
        rewards: [f64; 4],
        balances: [Option<f64>; 4],
    ) {
        let mut inner = self.lock();
        inner.outbox.push_back(json!({
            "type": "stat_event",
            "seq": seq,
            "at": at,
            "source": source,
            "message": message,
            "rewards": rewards,
            "balances": balances.map(|b| b.unwrap_or(0.0)),
        }));
        if inner.outbox.len() > MAX_OUTBOX {
            inner.outbox.pop_front();
        }
    }

    /// Echo `config` after local edits; coalesced, only the newest body is
    /// sent when the agent next flushes.
    pub fn push_config(&self, config: Value) {
        let mut inner = self.lock();
        if inner.config_sent.as_ref() != Some(&config) {
            inner.config_next = Some(config);
        }
    }

    /// Answer a queued panel command; delivered as `command_result`.
    pub fn reply(&self, id: &str, ok: bool, error: Option<String>, data: Option<Value>) {
        let mut inner = self.lock();
        let mut msg = json!({ "type": "command_result", "id": id, "ok": ok });
        if let Some(error) = error {
            msg["error"] = json!(error);
        }
        if let Some(data) = data {
            msg["data"] = data;
        }
        inner.outbox.push_back(msg);
    }

    /// Give the agent the session's client-log buffer. The cursor resets so
    /// only lines written after attach stream to the panel.
    pub fn set_logs(&self, logs: Arc<Mutex<VecDeque<String>>>) {
        let mut inner = self.lock();
        inner.log_cursor = logs.lock().map(|lines| lines.len()).unwrap_or(0);
        inner.logs = Some(logs);
    }

    /// Commands pulled by the UI tick; the agent refills them.
    pub fn take_commands(&self) -> Vec<PanelCommand> {
        self.lock().commands.drain(..).collect()
    }

    /// `stat_ack` cursors pulled by the statistics owner; the agent queues
    /// them as they arrive.
    pub fn take_stat_acks(&self) -> Vec<u64> {
        self.lock().stat_acks.drain(..).collect()
    }

    /// `(phase, detail)` for the settings UI — e.g. `("online", "")`.
    pub fn state(&self) -> (&'static str, String) {
        let inner = self.lock();
        (inner.phase, inner.detail.clone())
    }

    // ------------------------------------------------------------------
    // Agent-thread side

    fn set_phase(&self, phase: &'static str, detail: String) {
        let mut inner = self.lock();
        inner.phase = phase;
        inner.detail = detail;
    }

    fn settings_revision(&self) -> (PanelSettings, u64) {
        let inner = self.lock();
        (inner.settings.clone(), inner.revision)
    }

    fn take_frame(&self) -> Option<Value> {
        self.lock().frame.take()
    }

    fn peek_status(&self) -> Option<Value> {
        self.lock().status.clone()
    }

    fn take_outbox(&self) -> VecDeque<Value> {
        std::mem::take(&mut self.lock().outbox)
    }

    /// The pending `config` body if it differs from the last sent one; marks
    /// it sent optimistically (reconnects re-send via `hello`).
    fn take_config(&self) -> Option<Value> {
        let mut inner = self.lock();
        let next = inner.config_next.take()?;
        if inner.config_sent.as_ref() == Some(&next) {
            return None;
        }
        inner.config_sent = Some(next.clone());
        Some(next)
    }

    fn hello(&self) -> Value {
        self.lock().hello.clone().unwrap_or_else(|| {
            json!({
                "instance": {"server": "", "user_id": "", "username": ""},
                "client": "ruffle",
                "backend": "native",
                "modules": [],
                "config": {"active_module": Value::Null, "modules": {}},
            })
        })
    }

    fn push_command(&self, id: String, action: String, payload: Value) {
        self.lock().commands.push_back(PanelCommand {
            id,
            action,
            payload,
        });
    }

    /// New client-log lines since the last drain.
    fn drain_logs(&self) -> Vec<String> {
        let mut inner = self.lock();
        let Some(logs) = inner.logs.clone() else {
            return Vec::new();
        };
        let Ok(lines) = logs.lock() else {
            return Vec::new();
        };
        let total = lines.len();
        if inner.log_cursor > total {
            inner.log_cursor = 0;
        }
        let new: Vec<String> = lines
            .iter()
            .skip(inner.log_cursor)
            .take(512)
            .map(|line| line.chars().take(MAX_LINE).collect())
            .collect();
        inner.log_cursor = total.min(inner.log_cursor + new.len());
        new
    }
}

/// Spawn the reconnecting agent thread. The thread exits when
/// [`PanelLink::shutdown`] is set (on view drop / app quit).
pub fn spawn_agent(link: Arc<PanelLink>) {
    std::thread::Builder::new()
        .name("xenomit-panel-agent".into())
        .spawn(move || {
            let mut backoff = Duration::from_secs(1);
            loop {
                if link.is_shutdown() {
                    return;
                }
                let (settings, revision) = link.settings_revision();
                if !settings.enabled || settings.url.is_empty() || settings.token.is_empty() {
                    link.online.store(false, Ordering::Relaxed);
                    link.set_phase("disabled", String::new());
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                link.set_phase("connecting", String::new());
                let started = Instant::now();
                match run_socket(&link, &settings, revision) {
                    Ok(()) => link.set_phase("offline", "Connection closed".into()),
                    Err(error) => link.set_phase("offline", error),
                }
                link.online.store(false, Ordering::Relaxed);
                if link.is_shutdown() {
                    return;
                }
                // A long-lived session resets the backoff; fast failures grow it.
                if started.elapsed() > Duration::from_secs(15) {
                    backoff = Duration::from_secs(1);
                } else {
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                // Wait out the backoff but wake early on config changes or
                // shutdown so re-pairing reconnects immediately.
                let deadline = Instant::now() + backoff;
                while Instant::now() < deadline {
                    if link.is_shutdown() {
                        return;
                    }
                    if link.settings_revision().1 != revision {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .expect("spawn panel agent thread");
}

/// `http(s)://host[:port]` → `ws(s)://host[:port]/api/agent/ws`.
fn ws_url(base: &str) -> Result<String, String> {
    let base = base.trim().trim_end_matches('/');
    if base.starts_with("ws://") || base.starts_with("wss://") {
        return Ok(format!("{base}/api/agent/ws"));
    }
    if let Some(rest) = base.strip_prefix("https://") {
        return Ok(format!("wss://{rest}/api/agent/ws"));
    }
    if let Some(rest) = base.strip_prefix("http://") {
        return Ok(format!("ws://{rest}/api/agent/ws"));
    }
    // Bare host defaults to TLS, matching the panel's production deployment.
    Ok(format!("wss://{base}/api/agent/ws"))
}

fn set_read_timeout(stream: &mut tungstenite::stream::MaybeTlsStream<TcpStream>) {
    use tungstenite::stream::MaybeTlsStream as S;
    match stream {
        S::Plain(tcp) => {
            let _ = tcp.set_read_timeout(Some(READ_TIMEOUT));
        }
        S::NativeTls(tls) => {
            let _ = tls.get_mut().set_read_timeout(Some(READ_TIMEOUT));
        }
        _ => {}
    }
}

/// One authenticated connection attempt: handshake → hello → send/receive
/// loop until close, error, shutdown or a settings revision change.
fn run_socket(
    link: &Arc<PanelLink>,
    settings: &PanelSettings,
    revision: u64,
) -> Result<(), String> {
    let url = ws_url(&settings.url)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("Invalid panel URL {url}: {e}"))?;
    let auth = format!("Bearer {}", settings.token);
    request.headers_mut().insert(
        tungstenite::http::header::AUTHORIZATION,
        tungstenite::http::HeaderValue::from_str(&auth)
            .map_err(|_| "Instance token contains invalid characters".to_owned())?,
    );
    let (mut socket, _response) =
        tungstenite::connect(request).map_err(|e| format!("Connect failed: {e}"))?;
    set_read_timeout(socket.get_mut());
    link.online.store(true, Ordering::Relaxed);
    link.set_phase("online", String::new());

    let send =
        |socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
         body: Value|
         -> Result<(), String> {
            let text = serde_json::to_string(&body).map_err(|e| e.to_string())?;
            socket
                .send(tungstenite::Message::Text(text.into()))
                .map_err(|e| e.to_string())
        };

    let mut hello = link.hello();
    hello["type"] = json!("hello");
    hello["protocol"] = json!(PROTOCOL_VERSION);
    hello["agent_version"] = json!(AGENT_VERSION);
    hello["os"] = json!(std::env::consts::OS);
    if hello.get("machine").is_none() {
        let machine = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_default();
        if !machine.is_empty() {
            hello["machine"] = json!(machine);
        }
    }
    send(&mut socket, hello)?;

    let mut hz: u64 = 4;
    let mut seq: u64 = 0;
    let mut next_frame = Instant::now();
    let mut next_status = Instant::now();
    let mut next_keepalive = Instant::now() + KEEPALIVE;

    loop {
        if link.is_shutdown() {
            let _ = socket.close(None);
            return Ok(());
        }
        if link.settings_revision().1 != revision {
            // Re-pair/reconfigure happened underneath us.
            let _ = socket.close(None);
            return Ok(());
        }
        for msg in link.take_outbox() {
            send(&mut socket, msg)?;
        }
        if let Some(config) = link.take_config() {
            send(&mut socket, json!({ "type": "config", "config": config }))?;
        }
        let now = Instant::now();
        if now >= next_frame {
            next_frame = now + Duration::from_millis(1000 / hz.clamp(1, 10));
            if let Some(mut frame) = link.take_frame() {
                seq += 1;
                frame["type"] = json!("frame");
                frame["seq"] = json!(seq);
                send(&mut socket, frame)?;
            }
        }
        if now >= next_status {
            next_status = now + Duration::from_secs(1);
            if let Some(mut status) = link.peek_status() {
                status["type"] = json!("status");
                send(&mut socket, status)?;
            }
        }
        for line in link.drain_logs() {
            send(
                &mut socket,
                json!({"type": "log", "at": now_ms(), "level": "info", "line": line}),
            )?;
        }
        if now >= next_keepalive {
            next_keepalive = now + KEEPALIVE;
            socket
                .send(tungstenite::Message::Ping(Vec::new().into()))
                .map_err(|e| e.to_string())?;
        }
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                match msg["type"].as_str() {
                    Some("command") => {
                        let id = msg["id"].as_str().unwrap_or_default().to_owned();
                        let action = msg["action"].as_str().unwrap_or_default().to_owned();
                        if !id.is_empty() && !action.is_empty() {
                            link.push_command(
                                id,
                                action,
                                msg.get("payload").cloned().unwrap_or(json!({})),
                            );
                        }
                    }
                    Some("set_stream_rate") => {
                        if let Some(rate) = msg["hz"].as_u64() {
                            hz = rate.clamp(1, 10);
                        }
                    }
                    Some("stat_ack") => {
                        if let Some(cursor) = msg["cursor"].as_u64() {
                            link.lock().stat_acks.push_back(cursor);
                        }
                    }
                    Some("ping") => {
                        send(&mut socket, json!({"type": "pong", "t": msg["t"]}))?;
                    }
                    _ => {}
                }
            }
            Ok(tungstenite::Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(format!("Socket error: {e}")),
        }
    }
}

/// `POST /api/agent/pair` — exchange a short-lived pairing code for the
/// permanent instance token. Runs a tiny tokio runtime like the Backpage
/// balance fetch; safe to call from a background executor.
pub fn pair(base_url: &str, code: &str, instance: Value) -> Result<PairResponse, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("Panel URL is empty".into());
    }
    let url = format!("{base}/api/agent/pair");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;
        let response = client
            .post(&url)
            .json(&json!({ "code": code, "instance": instance }))
            .send()
            .await
            .map_err(|e| format!("Panel unreachable: {e}"))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(format!("Pairing failed (HTTP {status}): {detail}"));
        }
        serde_json::from_str::<PairResponse>(&text)
            .map_err(|e| format!("Bad pairing response: {e}"))
    })
}

#[derive(Clone, Debug, Deserialize)]
pub struct PairResponse {
    pub instance_id: String,
    pub token: String,
    #[allow(dead_code)]
    pub agent_ws_url: String,
}

/// Panel `game_command` payloads carry numeric fields as strings; coerce
/// them before deserializing into the typed command enum.
pub fn parse_game_command(payload: &Value) -> Result<xenomit_game_api::GameCommand, String> {
    let mut payload = payload.clone();
    if let Some(object) = payload.as_object_mut() {
        for key in [
            "portal_id",
            "gear_type",
            "target",
            "configuration",
            "repair_type",
            "gate_id",
            "spin_amount",
            "inviter_id",
        ] {
            if let Some(value) = object.get_mut(key)
                && let Some(text) = value.as_str()
                && let Ok(number) = text.parse::<i64>()
            {
                *value = json!(number);
            }
        }
    }
    serde_json::from_value(payload).map_err(|e| format!("Invalid game command: {e}"))
}

// ---------------------------------------------------------------------------
// LiveFrame → Agent Protocol v1 conversion
// ---------------------------------------------------------------------------

fn point(point: Point) -> Value {
    json!({ "x": point.x, "y": point.y })
}

fn faction_name(faction: i32) -> Option<&'static str> {
    match faction {
        1 => Some("mmo"),
        2 => Some("eic"),
        3 => Some("vru"),
        _ => None,
    }
}

/// DarkOrbit clan diplomacy ids → the panel's string vocabulary.
fn diplomacy_name(diplomacy: i32) -> &'static str {
    match diplomacy {
        1 | 2 => "ally", // alliance / non-aggression pact
        3 => "war",
        _ => "none",
    }
}

fn kind_name(kind: LiveKind) -> &'static str {
    match kind {
        LiveKind::Hero => "hero",
        LiveKind::Player => "player",
        LiveKind::Npc => "npc",
        LiveKind::Pet => "pet",
        LiveKind::Box => "box",
        LiveKind::Ore => "ore",
        LiveKind::Portal => "portal",
        LiveKind::Base => "base",
        LiveKind::Station => "station",
        LiveKind::Beacon => "beacon",
        LiveKind::Mine => "mine",
        LiveKind::Other => "other",
    }
}

fn entity_payload(entity: &LiveEntity) -> Value {
    let mut out = json!({
        "id": entity.id,
        "kind": kind_name(entity.kind),
        "x": entity.x,
        "y": entity.y,
        "is_npc": entity.is_npc,
        "is_hero": entity.is_hero,
        "is_collectable": entity.is_collectable,
        "clan_diplomacy": diplomacy_name(entity.clan_diplomacy),
    });
    if !entity.name.is_empty() {
        out["name"] = json!(entity.name);
    }
    if let Some(dest) = entity.dest {
        out["dest"] = point(dest);
    }
    if entity.has_hp {
        out["hp"] = json!(entity.hp);
        out["max_hp"] = json!(entity.max_hp);
        out["shield"] = json!(entity.shield);
        out["max_shield"] = json!(entity.max_shield);
    }
    if entity.has_type_id {
        out["type_id"] = json!(entity.type_id.to_string());
    }
    if let Some(faction) = faction_name(entity.faction) {
        out["faction"] = json!(faction);
    }
    if entity.clan != 0 {
        out["clan"] = json!(entity.clan.to_string());
    }
    out
}

fn hero_payload(frame: &LiveFrame) -> Option<Value> {
    let hero = frame.hero.as_ref()?;
    let entity = frame.entities.iter().find(|e| e.is_hero);
    let nav = &frame.navigation.hero;
    let fields = frame.navigation.entity_fields.get(&hero.id);
    let field = |name: &str| {
        fields
            .and_then(|fields| fields.get(name))
            .and_then(Value::as_f64)
    };
    let hp = |pick: fn(&LiveEntity) -> i32| entity.map(pick).unwrap_or_default();
    let killed = nav.killed || entity.is_some_and(|e| e.has_hp && e.max_hp > 0 && e.hp <= 0);
    let mut out = json!({
        "id": hero.id,
        "x": hero.x,
        "y": hero.y,
        "hp": hp(|e| e.hp),
        "max_hp": hp(|e| e.max_hp),
        "shield": hp(|e| e.shield),
        "max_shield": hp(|e| e.max_shield),
        "nanohull": entity.map(|e| e.nanohull).unwrap_or_default(),
        "nanohull_max": entity.map(|e| e.nanohull_max).unwrap_or_default(),
        "configuration": nav.configuration.unwrap_or(1),
        "killed": killed,
        "pet": {
            "owned": nav.owns_pet,
            "active": nav.pet_active,
            "destroyed": nav.pet_destroyed,
            "gear": nav.pet_gear.map(|gear| gear.to_string()),
        },
    });
    if let Some(dest) = hero.dest {
        out["dest"] = point(dest);
    }
    if let Some(faction) = entity
        .and_then(|e| faction_name(e.faction))
        .or_else(|| nav.faction.and_then(faction_name))
    {
        out["faction"] = json!(faction);
    }
    if entity.is_some_and(|e| e.clan != 0) {
        out["clan"] = json!(entity.map(|e| e.clan.to_string()).unwrap_or_default());
    }
    if let Some(level) = nav.level {
        out["level"] = json!(level);
    }
    if let (Some(used), Some(max)) = (field("cargo"), field("maxCargo")) {
        out["cargo"] = json!({ "used": used, "max": max });
    } else if let Some(used) = field("cargo") {
        out["cargo"] = json!({ "used": used, "max": 0 });
    }
    Some(out)
}

/// Best-effort pilot name from the live hero entity, used to fill the
/// instance identity once the game reports it.
pub fn hero_name(frame: &LiveFrame) -> Option<String> {
    frame
        .entities
        .iter()
        .find(|e| e.is_hero && !e.name.is_empty())
        .map(|e| e.name.clone())
}

/// Safe places derived from portal/base/station entities, matching the
/// radii the snapshot conversion uses.
fn safe_places(frame: &LiveFrame) -> Vec<Value> {
    frame
        .entities
        .iter()
        .filter_map(|entity| {
            let (kind, radius) = match entity.kind {
                LiveKind::Portal => ("portal", 250.0),
                LiveKind::Station => ("battle_station", 250.0),
                LiveKind::Base => ("base", 1500.0),
                _ => return None,
            };
            Some(json!({
                "kind": kind,
                "name": entity.name,
                "x": entity.x,
                "y": entity.y,
                "radius": radius,
            }))
        })
        .collect()
}

fn frame_payload(frame: &LiveFrame) -> Value {
    let mut out = Map::new();
    out.insert("captured_at".into(), json!(frame.captured_unix_ms));
    if let Some(map) = &frame.map {
        let name = frame
            .navigation
            .star_system
            .map(map.id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| {
                if map.id <= 0 {
                    "Loading".into()
                } else {
                    map.id.to_string()
                }
            });
        out.insert(
            "map".into(),
            json!({
                "id": map.id,
                "name": name,
                "width": map.width,
                "height": map.height,
            }),
        );
    }
    if let Some(hero) = hero_payload(frame) {
        out.insert("hero".into(), hero);
    }
    out.insert(
        "entities".into(),
        json!(
            frame
                .entities
                .iter()
                .map(entity_payload)
                .collect::<Vec<_>>()
        ),
    );
    out.insert(
        "portals".into(),
        json!(
            frame
                .navigation
                .portals
                .iter()
                .map(|portal| {
                    let target = portal.target_map.unwrap_or(-1);
                    let name = frame
                        .navigation
                        .star_system
                        .map(target)
                        .map(|m| m.name.clone());
                    json!({
                        "id": portal.id,
                        "x": portal.position.x,
                        "y": portal.position.y,
                        "target_map": target,
                        "target_name": name,
                        "jumping": portal.jumping,
                    })
                })
                .collect::<Vec<_>>()
        ),
    );
    out.insert(
        "obstacles".into(),
        json!(frame
            .navigation
            .obstacles
            .iter()
            .map(|obstacle| {
                match &obstacle.shape {
                    ObstacleShape::Circle { center, radius } => json!({
                        "type": "circle",
                        "center": { "x": center.x, "y": center.y },
                        "radius": radius,
                        "inverted": obstacle.inverted,
                    }),
                    ObstacleShape::Rectangle { bounds } => json!({
                        "type": "rectangle",
                        "bounds": {
                            "x": bounds.x,
                            "y": bounds.y,
                            "w": bounds.width,
                            "h": bounds.height,
                        },
                        "inverted": obstacle.inverted,
                    }),
                    ObstacleShape::Polygon { points } => json!({
                        "type": "polygon",
                        "points": points.iter().map(|p| json!({"x": p.x, "y": p.y})).collect::<Vec<_>>(),
                        "inverted": obstacle.inverted,
                    }),
                }
            })
            .collect::<Vec<_>>()),
    );
    out.insert("safe_places".into(), json!(safe_places(frame)));
    Value::Object(out)
}

/// Build the `config.modules` map the panel expects: a flat
/// `descriptor.key → value` object per module. Only descriptor keys are
/// exported — the panel rejects unknown keys in `update_config`.
pub fn module_settings_flat(
    descriptor: &xenomit_game_api::ModuleDescriptor,
    settings: &Value,
) -> Map<String, Value> {
    let mut out = Map::new();
    for field in &descriptor.settings {
        let value = xenomit_game_api::setting_value(settings, &field.key)
            .cloned()
            .unwrap_or_else(|| field.default.clone());
        out.insert(field.key.clone(), value);
    }
    out
}

#[cfg(test)]
mod tests;
