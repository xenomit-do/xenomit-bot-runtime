use super::*;

#[test]
fn settings_without_backend_default_to_native() {
    // modules.json written before the backend selector existed must load.
    let settings: BotSettings = serde_json::from_str("{}").expect("settings deserialize");
    assert_eq!(settings.game_api_backend, ClientBackend::Native);
    assert_eq!(settings.client, ClientKind::Ruffle);
    assert_eq!(settings.flash_backend, FlashBackend::Cached);
    assert_eq!(settings.game_api_backend.label(), "Native (shared memory)");
}

#[test]
fn module_config_carries_backend_tick() {
    let mut settings = BotSettings::default();
    assert_eq!(settings.module_config().settings["tick_ms"], json!(150));
    settings.game_api_backend = ClientBackend::LegacyPipe;
    assert_eq!(settings.module_config().settings["tick_ms"], json!(750));
    settings.client = ClientKind::Packet;
    assert_eq!(settings.module_config().settings["tick_ms"], json!(150));
}

#[test]
fn dynamic_module_settings_are_isolated_and_preserve_legacy_defaults() {
    let mut settings = BotSettings::default();
    settings.modules.insert(
        "custom_gate".into(),
        json!({"gate": {"count": 100}, "features": {"collect_boxes": false}}),
    );
    settings
        .modules
        .insert("custom_event".into(), json!({"event": "winter"}));
    let gate = settings.module_config_for("custom_gate");
    assert_eq!(gate.settings["gate"]["count"], json!(100));
    assert_eq!(gate.settings["features"]["collect_boxes"], json!(false));
    assert_eq!(gate.settings["features"]["collect_cargo"], json!(true));
    let event = settings.module_config_for("custom_event");
    assert_eq!(event.settings["event"], json!("winter"));
    assert!(event.settings.get("gate").is_none());
    let restored: BotSettings =
        serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
    assert_eq!(restored.module_config_for("custom_gate"), gate);
}

#[test]
fn flash_client_preserves_ruffle_preference_and_uses_fast_tick() {
    let settings: BotSettings =
        serde_json::from_str(r#"{"client":"flash","game_api_backend":"legacy_pipe"}"#).unwrap();
    assert_eq!(settings.client, ClientKind::Flash);
    assert_eq!(settings.game_api_backend, ClientBackend::LegacyPipe);
    assert_eq!(settings.module_config().settings["tick_ms"], 150);
    assert_eq!(
        settings
            .client
            .backend_label(settings.game_api_backend, settings.flash_backend),
        "Flash (cached state)"
    );
    let round_trip: BotSettings =
        serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
    assert_eq!(round_trip.client, ClientKind::Flash);
}

#[test]
fn client_choice_preserves_ruffle_backend() {
    let settings: BotSettings =
        serde_json::from_str(r#"{"client":"packet","game_api_backend":"legacy_pipe"}"#)
            .unwrap();
    let restored: BotSettings =
        serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
    assert_eq!(restored.client, ClientKind::Packet);
    assert_eq!(restored.game_api_backend, ClientBackend::LegacyPipe);
}

#[test]
fn file_store_round_trips_and_missing_file_yields_defaults() {
    let directory =
        std::env::temp_dir().join(format!("xenomit-settings-{}", std::process::id()));
    let path = directory.join("modules.json");
    let store = FileSettingsStore::new(path.clone());
    assert_eq!(store.load().active_module, "kill_collect");
    let settings = BotSettings {
        active_module: "collect".into(),
        ..Default::default()
    };
    store.save(&settings).unwrap();
    assert_eq!(store.load().active_module, "collect");
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
