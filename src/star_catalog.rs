//! Dynamic star-map catalog: fetches the same `maps-config` and
//! `spacemap-config` documents the game client loads from the CDN, so map
//! names and portal jump links track game updates instead of relying on
//! the bundled `star_map` table (which stays as the offline fallback).
//!
//! URL chain mirrors `InitGameFileCollectionCommand`:
//! `{cdn}{basePath}xml/resources.xml?__cv={resourcesXmlHash}` indexes every
//! resource file; each `<file>` resolves to
//! `{base}{location.path}{name}.{type}[?__cv={hash}]`.

use std::collections::HashMap;
use std::time::Duration;

use xenomit_backpage::session::Session;
use xenomit_game_api::navigation::StarSystem;

/// `{cdn}{basePath}` — the resource prefix `FileCollection` builds from
/// `Settings.staticHost + Settings.basePath`. `cdn` is optional in the
/// flashvars; the dynamic host and finally the instance origin stand in.
fn resource_base(session: &Session) -> String {
    let vars = &session.flashvars;
    let host = ["cdn", "dynamicHost", "host"]
        .iter()
        .find_map(|key| vars.get(*key))
        .map(String::as_str)
        .map(str::trim)
        .unwrap_or("");
    let host = if host.is_empty() || host == "/" {
        session.instance()
    } else if let Some(rest) = host.strip_prefix("//") {
        format!("https://{rest}")
    } else if host.starts_with("http://") || host.starts_with("https://") {
        host.to_owned()
    } else {
        format!("https://{host}")
    };
    let base = vars
        .get("basePath")
        .map(String::as_str)
        .unwrap_or("spacemap/")
        .trim_matches('/');
    if base.is_empty() {
        format!("{}/", host.trim_end_matches('/'))
    } else {
        format!("{}/{}/", host.trim_end_matches('/'), base)
    }
}

/// URL of a `resources.xml` file id: `{base}{location.path}{name}.{type}`
/// plus `?__cv={hash}` when the `hash` attribute exists.
fn file_url(resources_xml: &str, base: &str, id: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(resources_xml).ok()?;
    let locations: HashMap<&str, &str> = doc
        .descendants()
        .filter(|n| n.has_tag_name("location"))
        .filter_map(|n| Some((n.attribute("id")?, n.attribute("path").unwrap_or_default())))
        .collect();
    let file = doc
        .descendants()
        .find(|n| n.has_tag_name("file") && n.attribute("id") == Some(id))?;
    let path = file
        .attribute("location")
        .and_then(|loc| locations.get(loc).copied())
        .unwrap_or_default();
    let name = file.attribute("name").filter(|n| !n.is_empty())?;
    let kind = file.attribute("type").filter(|t| !t.is_empty())?;
    let mut url = format!("{base}{path}{name}.{kind}");
    if let Some(hash) = file.attribute("hash") {
        url.push_str("?__cv=");
        url.push_str(hash);
    }
    Some(url)
}

/// Fetch `maps-config` + `spacemap-config` for the session and build the
/// catalog with [`StarSystem::from_config_xml`]. Returns `None` on any
/// failure — callers fall back to the bundled static catalog.
pub async fn fetch(session: &Session) -> Option<StarSystem> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(xenomit_backpage::client::USER_AGENT)
        .build()
        .ok()?;
    let base = resource_base(session);
    let hash = session
        .flashvars
        .get("resourcesXmlHash")
        .cloned()
        .unwrap_or_default();
    let get = |url: String| {
        let http = &http;
        async move {
            let mut request = http.get(&url);
            if reqwest::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .as_deref()
                == Some(session.server.as_str())
            {
                request = request.header(reqwest::header::COOKIE, format!("dosid={}", session.sid));
            }
            let bytes = request
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .bytes()
                .await
                .ok()?;
            crate::packet_client::decode_http_body(&bytes).ok()
        }
    };
    let resources = get(format!("{base}xml/resources.xml?__cv={hash}")).await?;
    let maps = get(file_url(&resources, &base, "maps-config")?).await?;
    let space = get(file_url(&resources, &base, "spacemap-config")?).await?;
    StarSystem::from_config_xml(&maps, &space).ok()
}

#[cfg(test)]
mod tests;
