//! `BotSettings` — the persisted `modules.json` shape — plus the
//! [`SettingsStore`] persistence seam. The headless config embeds this same
//! shape (orchestrator doc §2.2), so it lives here rather than in the GUI.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;
use xenomit_behavior_engine::BehaviorConfig;
use xenomit_game_api::{ClientBackend, ModuleConfig};

use crate::panel::PanelSettings;

/// A user-drawn rectangular work area on a specific game map.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MapZone {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub enabled: bool,
    /// `preferred` zones bias target selection; `avoided` zones are hard movement and target exclusions.
    #[serde(default)]
    pub kind: MapZoneKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MapZoneKind {
    #[default]
    Preferred,
    Avoided,
}

impl Default for MapZone {
    fn default() -> Self {
        Self {
            name: String::new(),
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
            enabled: true,
            kind: MapZoneKind::Preferred,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    #[default]
    Ruffle,
    Flash,
    Packet,
}

impl ClientKind {
    pub fn backend_label(self, backend: ClientBackend, flash: FlashBackend) -> &'static str {
        match self {
            Self::Flash => flash.label(),
            Self::Packet => "Packet (TCP)",
            Self::Ruffle => backend.label(),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Ruffle => "Ruffle",
            Self::Flash => "Flash (OCX)",
            Self::Packet => "Packet client",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlashBackend {
    #[default]
    Cached,
    NativeMemory,
}
impl FlashBackend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cached => "Flash (cached state)",
            Self::NativeMemory => "Flash (native memory)",
        }
    }
    pub fn toggle(self) -> Self {
        match self {
            Self::Cached => Self::NativeMemory,
            Self::NativeMemory => Self::Cached,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BotSettings {
    /// Independent settings payloads for every discovered/custom module.
    pub modules: BTreeMap<String, serde_json::Value>,
    /// Global behavior rules that apply alongside the selected objective.
    #[serde(default)]
    pub behaviors: serde_json::Value,
    /// Global plugin enablement, shared by objective modules and rules.
    #[serde(default)]
    pub plugins: BTreeMap<String, bool>,
    /// Named objective configurations used by global profile rules.
    #[serde(default)]
    pub behavior_profiles: BTreeMap<String, ModuleConfig>,
    /// Last profile selected by a global rule, preventing repeated restart loops.
    #[serde(default)]
    pub active_behavior_profile: String,
    /// Rule tags applied to player IDs by group/player rules.
    #[serde(default)]
    pub player_tags: BTreeMap<i32, String>,
    pub client: ClientKind,
    pub active_module: String,
    pub preferred_zone: String,
    pub working_map: String,
    pub offensive_config: u8,
    pub roam_config: u8,
    pub run_config: u8,
    pub repair_config: u8,
    pub safety_enabled: bool,
    pub min_health: u8,
    pub max_health: u8,
    pub roaming_repair_health: u8,
    pub repair_shield: u8,
    pub max_deaths: u8,
    pub revive_location: String,
    pub revive_wait_seconds: u8,
    pub instant_repair_above: u16,
    pub feature_flags: BTreeMap<String, bool>,
    /// NPC species the killer modules target — auto-populated from live
    /// frames the first time each species is seen, so the list persists
    /// across restarts. `true` marks a species as killable; an empty map
    /// attacks every NPC.
    #[serde(default)]
    pub npcs: BTreeMap<String, bool>,
    /// Bonus box names the collector may pick up, discovered from live frames.
    #[serde(default)]
    pub boxes: BTreeMap<String, bool>,
    /// Ore/resource names the collector may pick up, discovered from live frames.
    #[serde(default)]
    pub collectables: BTreeMap<String, bool>,
    /// User-assigned rectangular work areas keyed by map ID.
    #[serde(default)]
    pub map_zones: BTreeMap<i32, Vec<MapZone>>,
    #[serde(default)]
    pub game_api_backend: ClientBackend,
    #[serde(default)]
    pub flash_backend: FlashBackend,
    #[serde(default)]
    pub behavior_config: Option<BehaviorConfig>,
    /// Web panel pairing: base URL, permanent instance token and the last
    /// known game identity. The token is stored here like the Backpage
    /// session file and is never logged.
    #[serde(default)]
    pub panel: PanelSettings,
}

impl Default for BotSettings {
    fn default() -> Self {
        Self {
            modules: BTreeMap::new(),
            behaviors: json!({}),
            plugins: BTreeMap::new(),
            behavior_profiles: BTreeMap::new(),
            active_behavior_profile: String::new(),
            player_tags: BTreeMap::new(),
            client: ClientKind::Ruffle,
            active_module: "kill_collect".into(),
            preferred_zone: "1-8".into(),
            working_map: "1-8".into(),
            offensive_config: 1,
            roam_config: 2,
            run_config: 2,
            repair_config: 1,
            safety_enabled: true,
            min_health: 40,
            max_health: 95,
            roaming_repair_health: 50,
            repair_shield: 100,
            max_deaths: 10,
            revive_location: "Base".into(),
            revive_wait_seconds: 5,
            instant_repair_above: 500,
            npcs: BTreeMap::new(),
            boxes: BTreeMap::new(),
            collectables: BTreeMap::new(),
            map_zones: BTreeMap::new(),
            game_api_backend: ClientBackend::default(),
            flash_backend: FlashBackend::default(),
            behavior_config: None,
            panel: PanelSettings::default(),
            feature_flags: BTreeMap::from([
                ("collect_cargo".into(), true),
                ("collect_palladium".into(), false),
                ("collect_boxes".into(), true),
                ("prioritize_npcs".into(), true),
                ("switch_configs".into(), true),
                ("retreat_low_health".into(), true),
                ("pet_active".into(), true),
                ("pet_auto_loot".into(), true),
                ("pet_resource_collector".into(), false),
                ("group_follow_leader".into(), false),
                ("group_assist".into(), false),
                ("group_invites".into(), false),
                ("plugin_engine_adapter".into(), false),
                ("plugin_client_bridge".into(), true),
                ("plugin_module_config".into(), true),
            ]),
        }
    }
}

impl BotSettings {
    pub fn module_config(&self) -> ModuleConfig {
        self.module_config_for(&self.active_module)
    }
    pub fn module_config_for(&self, id: &str) -> ModuleConfig {
        let mut config = ModuleConfig {
            module_id: id.to_owned(),
            settings: json!({
                "preferred_zone": self.preferred_zone,
                "working_map": self.working_map,
                "offensive_config": self.offensive_config,
                "roam_config": self.roam_config,
                "run_config": self.run_config,
                "repair_config": self.repair_config,
                "safety": {
                    "enabled": self.safety_enabled,
                    "min_health_percent": self.min_health,
                    "max_health_percent": self.max_health,
                    "roaming_repair_health_percent": self.roaming_repair_health,
                    "repair_shield_percent": self.repair_shield,
                    "max_deaths": self.max_deaths,
                    "revive_location": self.revive_location,
                    "revive_wait_seconds": self.revive_wait_seconds,
                    "instant_repair_above": self.instant_repair_above,
                },
                "features": self.feature_flags,
                "npcs": self.npcs,
                "plugins": self.plugins,
                "tick_ms": match (self.client, self.game_api_backend) {
                    (ClientKind::Packet | ClientKind::Flash, _) => 150,
                    (_, ClientBackend::Native) => 150,
                    (_, ClientBackend::LegacyPipe) => 750,
                },
                "behavior_config": self.behavior_config,
            }),
        };
        if let Some(settings) = self.modules.get(id) {
            merge_settings(&mut config.settings, settings);
        }
        // Shared target tables and assigned work areas are applied after the
        // objective settings so every module reads the same account-wide data.
        config.settings["npcs"] = serde_json::to_value(&self.npcs).unwrap_or_else(|_| json!({}));
        config.settings["boxes"] = serde_json::to_value(&self.boxes).unwrap_or_else(|_| json!({}));
        config.settings["collectables"] =
            serde_json::to_value(&self.collectables).unwrap_or_else(|_| json!({}));
        config.settings["map_zones"] =
            serde_json::to_value(&self.map_zones).unwrap_or_else(|_| json!({}));
        // Global rules are applied after module settings, so module-specific
        // payloads cannot replace or disable them accidentally.
        config.settings["behaviors"] = self.behaviors.clone();
        config.settings["plugins"] =
            serde_json::to_value(&self.plugins).unwrap_or_else(|_| json!({}));
        config
    }
}

fn merge_settings(target: &mut serde_json::Value, source: &serde_json::Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            let current = target.entry(key.clone()).or_insert(serde_json::Value::Null);
            if value.is_object() && current.is_object() {
                merge_settings(current, value);
            } else {
                *current = value.clone();
            }
        }
    }
}

/// Persistence seam for [`BotSettings`]. The GUI reads/writes a JSON file;
/// a headless front-end can serve a read-only config or route saves to the
/// control plane instead.
pub trait SettingsStore: Send + Sync {
    /// Load the stored settings; a missing or unreadable store yields
    /// defaults, matching the GUI's historical behavior.
    fn load(&self) -> BotSettings;
    /// Persist the settings.
    fn save(&self, settings: &BotSettings) -> Result<(), String>;
}

/// `%APPDATA%\XenomitBot` (platform equivalent) — the directory holding
/// modules.json, the client log, statistics journals and plugin manifests.
pub fn config_root() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join("XenomitBot")
}

/// The default [`SettingsStore`]: pretty-printed JSON at a fixed path.
pub struct FileSettingsStore {
    path: PathBuf,
}

impl FileSettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    /// `config_root()/modules.json`.
    pub fn default_path() -> PathBuf {
        config_root().join("modules.json")
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for FileSettingsStore {
    fn default() -> Self {
        Self::new(Self::default_path())
    }
}

impl SettingsStore for FileSettingsStore {
    fn load(&self) -> BotSettings {
        std::fs::read(&self.path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }
    fn save(&self, settings: &BotSettings) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let serialized = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
        std::fs::write(&self.path, serialized).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests;
