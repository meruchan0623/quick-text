//! The app's clipboard lease options are process-wide (`src-tauri` sets them
//! at startup from `prefs.json`), so setting them is tested here, in a test
//! process of its own: no unit test's lease can observe the change.
use quicker_platform::clipboard_options::{lease_options, set_lease_options};
use quicker_platform::traits::{LeaseOptions, RestorePolicy};
use std::time::Duration;

#[test]
fn options_set_at_startup_replace_the_defaults_for_later_leases() {
    assert_eq!(lease_options(), LeaseOptions::default(), "unset");
    let text_only = LeaseOptions {
        policy: RestorePolicy::TextOnly,
        restore_delay: Duration::from_millis(1200),
    };
    set_lease_options(text_only);
    assert_eq!(lease_options(), text_only);
    let full = LeaseOptions {
        policy: RestorePolicy::Full,
        restore_delay: Duration::from_millis(100),
    };
    set_lease_options(full);
    assert_eq!(lease_options(), full, "the latest setting wins");
}
