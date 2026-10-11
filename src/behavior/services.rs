use super::*;

pub(super) fn default_services() -> ServiceRegistry {
    let mut services = ServiceRegistryBuilder::new();
    services
        .register("backpage.skylab", skylab_service)
        .expect("skylab service");
    services
        .register("backpage.shop_purchase", shop_purchase_service)
        .expect("shop purchase service");
    services.build()
}

pub(super) static BACKPAGE: std::sync::OnceLock<Mutex<Option<xenomit_backpage::Session>>> =
    std::sync::OnceLock::new();

pub fn set_backpage_session(session: Option<xenomit_backpage::Session>) {
    if let Ok(mut slot) = BACKPAGE.get_or_init(|| Mutex::new(None)).lock() {
        *slot = session;
    }
}

pub(super) fn skylab_service(
    _context: xenomit_behavior_engine::ServiceContext,
    input: Value,
) -> Result<Value, String> {
    let session = BACKPAGE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "backpage session lock was poisoned".to_owned())?
        .clone()
        .ok_or("Backpage session is not connected")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let client =
            xenomit_backpage::BackpageClient::new(session).map_err(|error| error.to_string())?;
        let page = client
            .skylab_page()
            .await
            .map_err(|error| error.to_string())?;
        let rows = xenomit_behavior_engine::skylab_rows(&page);
        let enabled = input.get("modules").cloned().unwrap_or_else(|| json!({}));
        let buy_uri = input
            .get("buy_uri_robots")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match xenomit_behavior_engine::next_skylab_action(&rows, &enabled, buy_uri) {
            Some(SkylabAction::BuyRobots { construction, uri }) => {
                client
                    .buy_skylab_robot(&construction, uri)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(json!({ "status": format!("Bought skylab robots for {construction}") }))
            }
            Some(SkylabAction::Upgrade { path }) => {
                let path = path
                    .find("indexInternal")
                    .map(|index| path[index..].to_owned())
                    .unwrap_or(path);
                client
                    .get(&path, &[])
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(json!({ "status": format!("Started skylab upgrade {path}") }))
            }
            None => Ok(json!({ "status": "No skylab module is ready" })),
        }
    })
}

pub(super) fn shop_purchase_service(
    context: xenomit_behavior_engine::ServiceContext,
    input: Value,
) -> Result<Value, String> {
    if context.cancellation.is_cancelled() {
        return Err("Cancelled".into());
    }
    let category = input
        .get("category")
        .and_then(Value::as_str)
        .filter(|category| !category.trim().is_empty())
        .ok_or("Shop category is required")?;
    let item_id = input
        .get("item_id")
        .and_then(Value::as_str)
        .filter(|item_id| !item_id.trim().is_empty())
        .ok_or("Shop item ID is required")?;
    let amount = input
        .get("amount")
        .and_then(Value::as_u64)
        .filter(|amount| (1..=10_000_000).contains(amount))
        .ok_or("Shop amount must be between 1 and 10,000,000")?;
    let session = BACKPAGE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "backpage session lock was poisoned".to_owned())?
        .clone()
        .ok_or("Backpage session is not connected")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let client =
            xenomit_backpage::BackpageClient::new(session).map_err(|error| error.to_string())?;
        client
            .shop_purchase(category, item_id, amount)
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({ "status": "purchased", "item_id": item_id, "amount": amount }))
    })
}
