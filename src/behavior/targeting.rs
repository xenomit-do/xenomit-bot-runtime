use super::*;

pub(super) fn entity_label(target: &xenomit_behavior_engine::EntityRef) -> &str {
    if target.name.is_empty() {
        target.path.as_str()
    } else {
        target.name.as_str()
    }
}

pub(super) fn configured_map_zones(snapshot: &GameSnapshot, settings: &Value, kind: &str) -> Vec<(f64, f64, f64, f64)> {
    let Some(map_id) = snapshot
        .map
        .as_ref()
        .and_then(|map| map.id)
        .filter(|map| *map != -1)
        .or_else(|| snapshot.navigation.map_id.filter(|map| *map != -1))
    else {
        return Vec::new();
    };
    settings
        .get("map_zones")
        .and_then(|zones| zones.get(map_id.to_string().as_str()))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|zone| zone.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter(|zone| zone.get("kind").and_then(Value::as_str).unwrap_or("preferred") == kind)
        .filter_map(|zone| {
            let x = zone.get("x")?.as_f64()?;
            let y = zone.get("y")?.as_f64()?;
            let width = zone.get("width")?.as_f64()?;
            let height = zone.get("height")?.as_f64()?;
            (x.is_finite() && y.is_finite() && width.is_finite() && height.is_finite()).then_some((
                x.min(x + width),
                y.min(y + height),
                width.abs(),
                height.abs(),
            ))
        })
        .filter(|(_, _, width, height)| *width > 0.0 && *height > 0.0)
        .collect()
}

/// Mark targets outside assigned work areas invalid before the planner sees
/// them. This keeps the shared area rules effective for built-in and custom
/// objectives without changing their target-selection APIs.
pub(super) fn apply_global_target_constraints(snapshot: &mut GameSnapshot, settings: &Value) {
    let preferred_zones = configured_map_zones(snapshot, settings, "preferred");
    let avoided_zones = configured_map_zones(snapshot, settings, "avoided");
    for entity in &mut snapshot.entities {
        let entity_kind = entity
            .fields
            .get("entityKind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let class_name = entity.class_name.to_ascii_lowercase();
        let target = entity.category == xenomit_game_api::EntityCategory::Npc
            || matches!(entity_kind, "box" | "ore")
            || class_name.contains("bonusbox")
            || class_name == "box"
            || class_name == "ore";
        if !target {
            continue;
        }

        // Explicitly disabled entries apply to custom modules as well as the
        // built-in objective adapters. An unseen name remains allowed until
        // first-encounter discovery persists its default enabled entry.
        let list_name = if entity.category == xenomit_game_api::EntityCategory::Npc {
            Some("npcs")
        } else if matches!(entity_kind, "box")
            || class_name.contains("bonusbox")
            || class_name == "box"
        {
            Some("boxes")
        } else if matches!(entity_kind, "ore") || class_name == "ore" {
            Some("collectables")
        } else {
            None
        };
        if let Some(selection) = list_name
            .and_then(|name| settings.get(name))
            .and_then(Value::as_object)
            .and_then(|entries| {
                entries
                    .get(&entity.name)
                    .or_else(|| entries.get(&entity.class_name))
            })
            && selection.as_bool() == Some(false)
        {
            entity.fields.insert("isValid".into(), json!(false));
        }

        let inside_avoided = avoided_zones.iter().any(|(x, y, width, height)| {
            entity.position.x >= *x
                && entity.position.x <= *x + *width
                && entity.position.y >= *y
                && entity.position.y <= *y + *height
        });
        if !preferred_zones.is_empty() {
            let inside_preferred = preferred_zones.iter().any(|(x, y, width, height)| {
                entity.position.x >= *x && entity.position.x <= *x + *width
                    && entity.position.y >= *y && entity.position.y <= *y + *height
            });
            entity.fields.insert("inPreferredZone".into(), json!(inside_preferred));
        }
        if inside_avoided {
            entity.fields.insert("isValid".into(), json!(false));
        }
    }
}

pub(super) fn zone_roam_targets(
    snapshot: &GameSnapshot,
    settings: &Value,
) -> Vec<xenomit_behavior_engine::Point> {
    let zones = configured_map_zones(snapshot, settings, "preferred");
    if zones.is_empty() {
        return Vec::new();
    }
    let width = snapshot
        .map
        .as_ref()
        .and_then(|map| map.width)
        .unwrap_or(21_000.0);
    let height = snapshot
        .map
        .as_ref()
        .and_then(|map| map.height)
        .unwrap_or(13_100.0);
    let mut targets = Vec::with_capacity(zones.len() * 5);
    for (x, y, zone_width, zone_height) in zones {
        for (fx, fy) in [
            (0.5, 0.5),
            (0.25, 0.25),
            (0.75, 0.25),
            (0.75, 0.75),
            (0.25, 0.75),
        ] {
            targets.push(xenomit_behavior_engine::Point {
                x: (x + zone_width * fx).clamp(0.0, width),
                y: (y + zone_height * fy).clamp(0.0, height),
            });
        }
    }
    targets.extend(avoided_zone_escape_targets(snapshot, settings));
    targets
}

pub(super) fn avoided_zone_escape_targets(snapshot: &GameSnapshot, settings: &Value) -> Vec<xenomit_behavior_engine::Point> {
    let zones = configured_map_zones(snapshot, settings, "avoided");
    let Some(hero) = snapshot.entities.iter().find(|e| e.category == xenomit_game_api::EntityCategory::Player) else { return Vec::new() };
    let mut targets = Vec::new();
    for (x, y, w, h) in zones {
        if hero.position.x >= x && hero.position.x <= x+w && hero.position.y >= y && hero.position.y <= y+h {
            let pad = 250.0;
            targets.extend([
                (x-pad,y-pad), (x+w+pad,y-pad), (x-pad,y+h+pad), (x+w+pad,y+h+pad)
            ].into_iter().map(|(x,y)| xenomit_behavior_engine::Point { x: x.max(0.0), y: y.max(0.0) }));
        }
    }
    targets
}

pub(super) fn normalize_world_positions(snapshot: &mut GameSnapshot) {
    let hero = snapshot
        .entities
        .iter()
        .find(|e| e.category == xenomit_game_api::EntityCategory::Player);
    let hero_faction = snapshot
        .navigation
        .hero
        .faction
        .map(f64::from)
        .or_else(|| hero.and_then(|e| numeric_field(e, "faction")))
        .filter(|f| *f > 0.0);
    let hero_clan = hero
        .and_then(|e| numeric_field(e, "clanId").or_else(|| numeric_field(e, "clan")))
        .filter(|c| *c > 0.0);
    for entity in &mut snapshot.entities {
        if entity.category == xenomit_game_api::EntityCategory::Ship {
            if !entity.fields.contains_key("isEnemy")
                && let Some((faction, hero_faction)) = numeric_field(entity, "faction")
                    .filter(|f| *f > 0.0)
                    .zip(hero_faction)
            {
                entity.fields.insert(
                    "isEnemy".into(),
                    json!(
                        faction != hero_faction
                            || numeric_field(entity, "clanDiplomacy") == Some(3.0)
                    ),
                );
            }
            if !entity.fields.contains_key("isAlly") {
                let same_faction = numeric_field(entity, "faction")
                    .zip(hero_faction)
                    .is_some_and(|(a, b)| a == b);
                let same_clan = numeric_field(entity, "clanId")
                    .or_else(|| numeric_field(entity, "clan"))
                    .zip(hero_clan)
                    .is_some_and(|(a, b)| a == b);
                if same_faction || same_clan {
                    entity.fields.insert("isAlly".into(), json!(true));
                }
            }
            if entity.class_name == "Pet" {
                entity.fields.insert("isPet".into(), json!(true));
            }
        }
        if let Some(id) = entity
            .id
            .as_ref()
            .and_then(Value::as_i64)
            .and_then(|id| i32::try_from(id).ok())
            && let Some(fields) = snapshot.navigation.entity_fields.get(&id)
        {
            entity.fields.extend(fields.clone());
        }
        if !entity.fields.contains_key("clanId")
            && let Some(clan) = entity.fields.get("clan").cloned()
        {
            entity.fields.insert("clanId".into(), clan);
        }
        if entity.category == xenomit_game_api::EntityCategory::Player
            && !entity.fields.contains_key("isGG")
            && let Some(name) = snapshot
                .navigation
                .map_id
                .and_then(|id| snapshot.navigation.star_system.map(id))
                .map(|m| m.name.as_str())
        {
            if name.starts_with("GG ") {
                entity.fields.insert("isGG".into(), json!(true));
            } else if name.len() == 3 && name.as_bytes()[1] == b'-' {
                entity.fields.insert("isGG".into(), json!(false));
            }
        }
        let Some(x) = numeric_field(entity, "worldX") else {
            continue;
        };
        let Some(y) = numeric_field(entity, "worldY") else {
            continue;
        };
        if x.is_finite() && y.is_finite() {
            entity.position.x = x;
            entity.position.y = y;
        }
    }
}

pub(super) fn numeric_field(entity: &GameEntity, name: &str) -> Option<f64> {
    entity.fields.get(name).and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
    })
}

pub(super) fn clip_destination_away_from_zones(
    snapshot: &GameSnapshot,
    settings: &Value,
    from: xenomit_behavior_engine::Point,
    destination: xenomit_behavior_engine::Point,
) -> xenomit_behavior_engine::Point {
    for (x, y, w, h) in configured_map_zones(snapshot, settings, "avoided") {
        let inside = |p: xenomit_behavior_engine::Point| p.x >= x && p.x <= x+w && p.y >= y && p.y <= y+h;
        if !inside(destination) { continue; }
        let pad = 250.0;
        let candidates = [(x-pad,y-pad), (x+w+pad,y-pad), (x-pad,y+h+pad),(x+w+pad,y+h+pad)];
        let Some((tx,ty)) = candidates.into_iter().filter(|(tx,ty)| *tx >= 0.0 && *ty >= 0.0)
            .min_by(|a,b| ((a.0-from.x).hypot(a.1-from.y).total_cmp(&((b.0-from.x).hypot(b.1-from.y)))))
        else { return from };
        return xenomit_behavior_engine::Point { x: tx, y: ty };
    }
    destination
}
