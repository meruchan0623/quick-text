//! The running app's clipboard lease options (B-08). `src-tauri` sets them
//! once at startup from `prefs.json` (`app.clipboard`), before the engine
//! starts; every lease a paste opens afterwards, automatic or manual, uses
//! them. Until they are set, leases use [`LeaseOptions::default`].
use crate::traits::LeaseOptions;
use std::sync::{PoisonError, RwLock};

static OPTIONS: RwLock<Option<LeaseOptions>> = RwLock::new(None);

/// Sets the options every later lease opens with. A lease already open keeps
/// the options it was opened with.
pub fn set_lease_options(options: LeaseOptions) {
    *OPTIONS.write().unwrap_or_else(PoisonError::into_inner) = Some(options);
    tracing::info!(
        event = "clipboard.options_set",
        policy = ?options.policy,
        restore_delay_ms = u64::try_from(options.restore_delay.as_millis()).unwrap_or(u64::MAX),
        "clipboard lease options set"
    );
}

/// The options the next lease opens with.
pub fn lease_options() -> LeaseOptions {
    OPTIONS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .unwrap_or_default()
}
