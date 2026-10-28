//! Team phrase commands (docs/cloud-phrases.md §5). Every command returns the
//! `CloudState` after it; changes are also sent as `cloud-state`. Arguments are
//! snake_case like the other 0.4 commands.
use crate::{
    app_state::Managed,
    cloud::{self, CloudSettingsInput, CloudState, CloudSync, Trigger},
    error::CommandError,
};
use tauri::{AppHandle, Manager, State};

#[tauri::command(rename_all = "snake_case")]
pub fn get_cloud_state(store: State<Managed>) -> CloudState {
    store.lock().cloud_state()
}

/// Saves the settings (`key`: absent or null keeps the stored key, `""`
/// clears it); `validation` for an address that is not https (or http to
/// localhost), a key with characters a header cannot carry, or an interval
/// outside 0/15/30/60/120/240. With the subscription on, a sync starts right
/// after; this returns without waiting for it (its start and end are
/// `cloud-state` events).
#[tauri::command(rename_all = "snake_case")]
pub fn set_cloud_settings(
    settings: CloudSettingsInput,
    app: AppHandle,
    store: State<Managed>,
    hub: State<CloudSync>,
) -> Result<CloudState, CommandError> {
    let state = cloud::apply_settings(&store, &hub, settings)?;
    if state.settings.enabled {
        tauri::async_runtime::spawn(async move {
            let store = app.state::<Managed>();
            let hub = app.state::<CloudSync>();
            cloud::sync(&store, &hub, Trigger::Settings).await;
        });
    }
    Ok(state)
}

/// Waits for the sync to end. A failed sync is not an error: see
/// `status.last_error`.
#[tauri::command(rename_all = "snake_case")]
pub async fn sync_cloud_now(
    store: State<'_, Managed>,
    hub: State<'_, CloudSync>,
) -> Result<CloudState, CommandError> {
    Ok(cloud::sync(&store, &hub, Trigger::Manual).await)
}

/// `ids` without the `cloud:` prefix.
#[tauri::command(rename_all = "snake_case")]
pub fn set_cloud_rules_disabled(
    ids: Vec<String>,
    disabled: bool,
    store: State<Managed>,
    hub: State<CloudSync>,
) -> Result<CloudState, CommandError> {
    cloud::set_rules_disabled(&store, &hub, ids, disabled)
}
