//! Packet transport and translation of the controller's game calls.
use std::collections::BTreeMap;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::statistics::{self, SharedStats};
use serde_json::json;
use xenomit_backpage::Session;
use xenomit_game_api::{
    ActionResult, ApiError, Bounds, ClientEventBatch, ClientFeatureState, ClientStatus,
    EntityCategory, FunctionCall, GameClient, GameCommand, GameEntity, GameSnapshot, Inspection,
    Point, Result,
    live::LiveFrame,
};
use xenomit_packet_client::{
    gameapi::PacketGameClient,
    session::{LoginInfo, Session as PacketSession},
};
use xenomit_packet_client::{
    packet::{Packet, Value},
    session::Event,
};

pub async fn connect(
    session: Session,
    statistics: SharedStats,
) -> std::result::Result<Arc<dyn GameClient>, String> {
    connect_with(session, statistics, None).await
}

/// [`connect`] with an optional `host:port` game endpoint that replaces the
/// `maps.php` lookup — a test seam for mock game servers and headless
/// daemons; `None` preserves the production maps.php resolution.
pub async fn connect_with(
    session: Session,
    statistics: SharedStats,
    game_endpoint: Option<&str>,
) -> std::result::Result<Arc<dyn GameClient>, String> {
    connect_via(session, statistics, game_endpoint, None).await
}

/// [`connect_with`] with an optional egress proxy. When `proxy` is set the
/// game socket is tunneled through it (HTTP `CONNECT`/SOCKS5, DNS at the
/// proxy) with no direct fallback — a tunnel failure returns the proxy
/// error verbatim so callers can classify exit 69 (docs/05 §5.4).
pub async fn connect_via(
    session: Session,
    statistics: SharedStats,
    game_endpoint: Option<&str>,
    proxy: Option<&xenomit_packet_client::proxy::ProxyConfig>,
) -> std::result::Result<Arc<dyn GameClient>, String> {
    let vars = &session.flashvars;
    let required = |key: &str| {
        vars.get(key)
            .filter(|v| !v.is_empty())
            .cloned()
            .ok_or_else(|| format!("Authenticated session is missing {key}"))
    };
    let mut login = LoginInfo::new(
        required("userID")?
            .parse::<i32>()
            .map_err(|_| "Invalid userID")?,
        required("sessionID")?,
    );
    // Factionless accounts do not receive a factionID flashvar; the stock
    // client leaves Hero.factionID at 0 and the login request sends 0.
    login.faction_id = vars
        .get("factionID")
        .filter(|v| !v.is_empty())
        .map(|v| v.parse().map_err(|_| "Invalid factionID".to_string()))
        .transpose()?
        .unwrap_or(0);
    login.instance_id = vars
        .get("pid")
        .or_else(|| vars.get("projectID"))
        .ok_or("Authenticated session is missing projectID")?
        .parse()
        .map_err(|_| "Invalid projectID")?;
    // `Settings.platformTypeClient == "PC_CLIENT"` in the stock client.
    login.is_mini_client = vars.get("platform").is_some_and(|v| v == "PC_CLIENT");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(xenomit_backpage::client::USER_AGENT)
        .build()
        .map_err(|e| e.to_string())?;
    // The bundled hash is a fixture. The live `VersionHash` is stamped
    // into each deployed main.swf and the server rejects anything else.
    if let Some(hash) = live_version_hash(&http, vars).await? {
        login.version = hash;
    }
    let endpoint = match game_endpoint {
        Some(endpoint) => endpoint.to_owned(),
        None => {
            let map_id = required("mapID")?;
            let base = vars
                .get("basePath")
                .map(String::as_str)
                .unwrap_or("spacemap/");
            let host = vars
                .get("dynamicHost")
                .or_else(|| vars.get("host"))
                .map(String::as_str)
                .unwrap_or("");
            let path = map_config_url(&session.instance(), host, base)?;
            let mut request = http.get(path.clone());
            if path.host_str() == Some(session.server.as_str()) {
                request = request.header(reqwest::header::COOKIE, format!("dosid={}", session.sid));
            }
            let response = request
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("Could not load map servers: {e}"))?;
            let bytes = response.bytes().await.map_err(|e| e.to_string())?;
            // The game server gzips maps.php even when the client did not
            // ask, and this crate's HTTP client does not decode
            // Content-Encoding. Sniff the magic, same as the backpage client.
            let body = decode_http_body(&bytes)?;
            map_endpoint(&body, &map_id)?
        }
    };
    let timeout = Duration::from_secs(15);
    let stream = match proxy {
        // Fail closed: the dial error is a typed proxy failure upstream —
        // surface it with a `proxy:` marker for exit-69 classification.
        Some(proxy) => xenomit_packet_client::proxy::dial_timeout(proxy, &endpoint, timeout)
            .map_err(|e| format!("proxy: {e}"))?,
        None => {
            let addresses = endpoint
                .to_socket_addrs()
                .map_err(|e| format!("Could not resolve map server: {e}"))?;
            let mut stream = None;
            for address in addresses {
                if let Ok(socket) = TcpStream::connect_timeout(&address, timeout) {
                    stream = Some(socket);
                    break;
                }
            }
            stream.ok_or("Could not connect to the map server")?
        }
    };
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let control = stream.try_clone().map_err(|e| e.to_string())?;
    let mut socket = PacketSession::new(stream);
    socket
        .handshake(&login)
        .map_err(|e| format!("Game login failed: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while socket.world.hero.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Game server did not initialize the hero within 30 seconds".into());
        }
        socket
            .stream
            .set_read_timeout(Some(remaining.min(timeout)))
            .map_err(|e| e.to_string())?;
        socket
            .recv()
            .map_err(|e| format!("Could not initialize the hero: {e}"))?;
    }
    socket
        .complete_ready(&login.view_data)
        .map_err(|e| e.to_string())?;
    let inner = Arc::new(PacketGameClient::new(socket));
    let stopped = Arc::new(AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None));
    let client = Arc::new(ControllerPacketClient {
        inner: inner.clone(),
        control,
        stopped: stopped.clone(),
        error: error.clone(),
        selected: Mutex::new(None),
        boxes: Mutex::new(BTreeMap::new()),
    });
    std::thread::Builder::new()
        .name("packet-session".into())
        .spawn(move || {
            let mut heartbeat = Instant::now();
            let mut assets_at = Instant::now() - Duration::from_secs(60);
            while !stopped.load(Ordering::Acquire) {
                let result = inner.pump_events(Duration::from_millis(20)).map(|events| {
                    if let Ok(mut stats) = statistics.lock() {
                        for event in events {
                            if let Event::Packet(packet) = event
                                && let Some(message) = game_log(&packet)
                            {
                                stats.log(&message, "packet", statistics::now_ms());
                            }
                        }
                        if assets_at.elapsed() >= Duration::from_secs(60) {
                            let balances = inner.with_world(|world| {
                                world.hero.as_ref().map(|hero| {
                                    [
                                        Some(hero.credits),
                                        Some(hero.uridium),
                                        Some(hero.honour),
                                        Some(hero.ep),
                                    ]
                                })
                            });
                            if let Some(balances) = balances {
                                stats.reconcile(balances, "packet", statistics::now_ms());
                                assets_at = Instant::now();
                            }
                        }
                    }
                });
                let result = result.and_then(|_| {
                    if heartbeat.elapsed() >= Duration::from_secs(20) {
                        heartbeat = Instant::now();
                        inner.ping()
                    } else {
                        Ok(())
                    }
                });
                if let Err(e) = result {
                    *error.lock().unwrap() = Some(e.to_string());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
        .map_err(|e| format!("Could not start packet monitor: {e}"))?;
    Ok(client)
}

/// Preserve localization keys and all replacements when the packet client
/// has no locale catalog. Only literal English reward text is interpreted.
fn game_log(packet: &Packet) -> Option<String> {
    if packet.name == Some("LegacyModule") {
        let text = packet.string("message").ok()?;
        return (text.starts_with("0|y|")
            || text.starts_with("0|LM|")
            || text.starts_with("L|n|MSG|"))
        .then(|| text.to_owned());
    }
    if packet.name != Some("LogMessageCommand") {
        return None;
    }
    let message = packet.module("message").ok()?;
    let mut text = message.string("baseKey").ok()?.to_owned();
    if let Ok(replacements) = message.list("wildCardReplacements") {
        for value in replacements {
            if let Value::Module(replacement) = value
                && let (Ok(key), Ok(value)) = (
                    replacement.string("wildcard"),
                    replacement.string("replacement"),
                )
            {
                if text.contains(key) {
                    text = text.replace(key, value);
                } else {
                    text.push_str(&format!(" {key}={value}"));
                }
            }
        }
    }
    Some(text)
}

fn map_config_url(
    instance: &str,
    host: &str,
    base: &str,
) -> std::result::Result<reqwest::Url, String> {
    let instance = reqwest::Url::parse(instance).map_err(|e| e.to_string())?;
    let host = if host.is_empty() || host == "/" {
        instance
    } else if host.starts_with("//") {
        reqwest::Url::parse(&format!("https:{host}")).map_err(|e| e.to_string())?
    } else if host.starts_with("http://") || host.starts_with("https://") {
        reqwest::Url::parse(host).map_err(|e| e.to_string())?
    } else if host.starts_with('/') {
        instance.join(host).map_err(|e| e.to_string())?
    } else {
        reqwest::Url::parse(&format!("https://{host}")).map_err(|e| e.to_string())?
    };
    let root = format!("{}/", host.as_str().trim_end_matches('/'));
    reqwest::Url::parse(&root)
        .and_then(|url| url.join(&format!("{}/xml/maps.php", base.trim_matches('/'))))
        .map_err(|e| e.to_string())
}

pub(crate) fn decode_http_body(bytes: &[u8]) -> std::result::Result<String, String> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        use std::io::Read;
        let mut out = String::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_string(&mut out)
            .map_err(|e| format!("Could not decompress the map server configuration: {e}"))?;
        Ok(out)
    } else {
        String::from_utf8(bytes.to_vec())
            .map_err(|e| format!("Map server configuration is not UTF-8: {e}"))
    }
}

fn map_endpoint(xml: &str, map_id: &str) -> std::result::Result<String, String> {
    let document = roxmltree::Document::parse(xml)
        .map_err(|e| format!("Invalid map server configuration: {e}"))?;
    let ip = document
        .descendants()
        .find(|n| n.has_tag_name("map") && n.attribute("id") == Some(map_id))
        .and_then(|n| n.children().find(|n| n.has_tag_name("gameserverIP")))
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("No game server for map {map_id}"))?;
    Ok(if ip.contains(':') {
        ip.to_owned()
    } else {
        format!("{ip}:5001")
    })
}

/// `{cdn}{gameclientPath|basePath}/main.swf?__cv={gameclientHash}`.
/// `None` when the flashvars do not name a deployed client (mocks, older
/// layouts) — callers then keep the bundled [`xenomit_packet_client::modules::VERSION_HASH`].
fn main_swf_url(vars: &std::collections::HashMap<String, String>) -> Option<reqwest::Url> {
    let cdn = vars
        .get("cdn")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())?;
    let hash = vars
        .get("gameclientHash")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())?;
    let path = vars
        .get("gameclientPath")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| vars.get("basePath"))
        .map(|s| s.trim().trim_matches('/'))
        .unwrap_or("spacemap");
    let cdn = cdn.trim_end_matches('/');
    let url = if path.is_empty() {
        format!("{cdn}/main.swf?__cv={hash}")
    } else {
        format!("{cdn}/{path}/main.swf?__cv={hash}")
    };
    reqwest::Url::parse(&url).ok()
}

async fn live_version_hash(
    http: &reqwest::Client,
    vars: &std::collections::HashMap<String, String>,
) -> std::result::Result<Option<String>, String> {
    let Some(url) = main_swf_url(vars) else {
        return Ok(None);
    };
    let bytes = http
        .get(url)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|e| format!("Could not download the game client: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("Could not download the game client: {e}"))?;
    version_hash_from_swf(&bytes)
        .map(Some)
        .ok_or_else(|| "Game client did not contain a login version hash".into())
}

/// `VersionHash.VERSION_HASH` from a CWS or FWS `main.swf`. The name and
/// its 32-hex value sit next to each other in the DoABC constant pool.
fn version_hash_from_swf(bytes: &[u8]) -> Option<String> {
    let body = swf_body(bytes)?;
    find_version_hash(&body)
}

fn swf_body(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < 8 {
        return None;
    }
    match &bytes[..3] {
        b"FWS" => Some(bytes[8..].to_vec()),
        b"CWS" => {
            use std::io::Read;
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(&bytes[8..])
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        _ => None,
    }
}

fn find_version_hash(data: &[u8]) -> Option<String> {
    const NAME: &[u8] = b"VERSION_HASH";
    let mut at = 0;
    while let Some(off) = data[at..].windows(NAME.len()).position(|w| w == NAME) {
        let start = at + off + NAME.len();
        let end = (start + 64).min(data.len());
        for i in start..end {
            if i + 32 > data.len() {
                break;
            }
            let candidate = &data[i..i + 32];
            if candidate
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            {
                return String::from_utf8(candidate.to_vec()).ok();
            }
        }
        at = start;
        if at >= data.len() {
            break;
        }
    }
    None
}

struct ControllerPacketClient {
    inner: Arc<PacketGameClient>,
    control: TcpStream,
    stopped: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    selected: Mutex<Option<i64>>,
    boxes: Mutex<BTreeMap<String, i32>>,
}

impl Drop for ControllerPacketClient {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.control.shutdown(Shutdown::Both);
    }
}

impl GameClient for ControllerPacketClient {
    fn status(&self) -> Result<ClientStatus> {
        if let Some(e) = self.error.lock().unwrap().as_ref() {
            return Err(ApiError::Transport(e.clone()));
        }
        self.inner.status()
    }
    /// Without this delegation the trait default answers `Unavailable`,
    /// which fails the whole behavior worker on its first typed command.
    fn game_command(&self, command: &GameCommand) -> Result<ActionResult> {
        self.status()?;
        self.inner.game_command(command)
    }
    fn feature_state(&self) -> Result<ClientFeatureState> {
        self.status()?;
        self.inner.feature_state()
    }
    fn poll_events(&self, after_sequence: u64) -> Result<ClientEventBatch> {
        self.status()?;
        self.inner.poll_events(after_sequence)
    }
    fn read_live_frame(&self, frame: &mut LiveFrame) -> bool {
        self.inner.read_live_frame(frame)
    }
    fn ping(&self) -> Result<()> {
        self.inner.ping()
    }
    fn inspect(&self, target: &str) -> Result<Inspection> {
        self.inner.inspect(target)
    }
    fn snapshot(&self, target: Option<&str>) -> Result<GameSnapshot> {
        self.status()?;
        let mut snapshot = self.inner.snapshot(target)?;
        let mut boxes = self.boxes.lock().unwrap();
        for entity in &mut snapshot.entities {
            if entity.category == EntityCategory::Player {
                entity.category = EntityCategory::Ship;
            }
            if entity.path.starts_with("collectable/") {
                let next = -(boxes.len() as i32) - 1;
                let hash = entity.name.clone();
                entity.id = Some(json!(*boxes.entry(hash.clone()).or_insert(next)));
                entity.fields.insert("hash".into(), json!(hash));
                entity.fields.insert("isCollectable".into(), json!(true));
                let kind = match entity.fields.get("kind").and_then(|v| v.as_str()) {
                    Some("Ore") => "ore",
                    Some("Box" | "FireworkBox") => "box",
                    Some("Beacon") => "beacon",
                    _ => "other",
                };
                entity.fields.insert("entityKind".into(), json!(kind));
                if let Some(value) = entity.fields.get("typeId").cloned() {
                    entity.fields.insert("typeID".into(), value);
                }
                entity.class_name =
                    if entity.fields.get("kind").and_then(|v| v.as_str()) == Some("Ore") {
                        "Ore"
                    } else {
                        "Box"
                    }
                    .into();
            }
        }
        self.inner.with_world(|world| {
            if let Some(h) = &world.hero {
                snapshot.entities.retain(|e| e.id != Some(json!(h.user_id)));
                snapshot.entities.push(GameEntity {
                    path: "hero".into(),
                    class_name: "Hero".into(),
                    name: h.user_name.clone(),
                    category: EntityCategory::Player,
                    position: Point {
                        x: h.x as f64,
                        y: h.y as f64,
                    },
                    bounds: Bounds::default(),
                    id: Some(json!(h.user_id)),
                    health: Some(h.hitpoints),
                    max_health: Some(h.hitpoints_max),
                    shield: Some(h.shield as f64),
                    max_shield: Some(h.shield_max as f64),
                    fields: serde_json::from_value(
                        json!({"isHero":true,"speed":h.speed,"x":h.x,"y":h.y,
                               "cargo":h.cargo_space,"maxCargo":h.cargo_space_max,
                               "localTargetId":h.target_id,"lockedTargetId":h.target_id}),
                    )
                    .unwrap(),
                });
            }
        });
        snapshot.visited = snapshot.entities.len();
        Ok(snapshot)
    }
    fn call_function(&self, call: &FunctionCall) -> Result<ActionResult> {
        self.status()?;
        let mut translated = call.clone();
        match call.function.as_str() {
            "moveHeroToCordinates" => {
                if let Some(id) = call
                    .arguments
                    .get(2)
                    .and_then(|v| v.get("$xenomit_entity_id"))
                    .and_then(|v| v.as_i64())
                {
                    let hash = self
                        .boxes
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(_, value)| i64::from(**value) == id)
                        .map(|(hash, _)| hash.clone())
                        .ok_or_else(|| ApiError::InvalidCall("Collectable has expired".into()))?;
                    let ore = self.inner.with_world(|w| {
                        w.collectables.get(&hash).is_some_and(|c| {
                            c.kind == xenomit_packet_client::model::CollectableKind::Ore
                        })
                    });
                    translated.function = if ore { "harvest" } else { "collect" }.into();
                    translated.arguments = vec![json!(hash)];
                } else {
                    translated.function = "move_to".into();
                    translated.arguments.truncate(2);
                }
            }
            "selectShip" => {
                translated.function = "select".into();
                translated.arguments.truncate(1);
                *self.selected.lock().unwrap() = call.arguments.first().and_then(|v| v.as_i64());
            }
            "sendNotification"
                if call.arguments.first().and_then(|v| v.as_str())
                    == Some("MapAssetNotificationAttackMapAsset") =>
            {
                translated.function = "attack".into();
                translated.arguments =
                    vec![json!(self.selected.lock().unwrap().ok_or_else(|| {
                        ApiError::InvalidCall("No selected NPC".into())
                    })?)];
            }
            "deselectSelectedShip" => {
                translated.function = "stop_attack".into();
                translated.arguments.clear();
                *self.selected.lock().unwrap() = None;
            }
            "stopHero" => {
                let position = self
                    .inner
                    .with_world(|w| w.hero_pos())
                    .ok_or_else(|| ApiError::InvalidCall("Hero is unavailable".into()))?;
                translated.function = "move_to".into();
                translated.arguments = vec![json!(position.0), json!(position.1)];
            }
            _ => {}
        }
        self.inner.call_function(&translated)
    }
}

#[cfg(test)]
mod tests;
