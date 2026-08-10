//! The Tilt side of keeping the dev environment current: when to show the nav
//! update button, and what a click does. The domain it drives —
//! [`repos_core::selfupdate`] — knows nothing about buttons.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use repos_core::selfupdate::{DevEnv, Update};
use repos_core::supervisor;

use crate::buttons;

/// Shows or hides the nav update button to match how far behind the dev-env
/// repo is. Its presence is the whole notification, so it's removed as soon as
/// there's nothing left to offer — someone pulled by hand, or the update landed.
pub fn refresh_button(dev: &DevEnv, fetch: bool) {
    if fetch {
        dev.fetch();
    }
    let behind = dev.behind();
    let result = if behind > 0 {
        buttons::render_update_button(behind, supervisor::marker().is_some())
    } else {
        buttons::remove_update_button()
    };
    match result {
        Ok(()) => tracing::debug!(behind, "dev-env update button refreshed"),
        Err(e) => tracing::error!(error = %e, "refreshing the dev-env update button failed"),
    }
}

/// Applies a pending dev-env update. On the unsupervised path nothing restarts
/// to redraw the buttons, so the now-stale "update available" is cleared here.
pub fn apply(dev: &DevEnv) {
    match dev.update() {
        Ok(Update::Restarting) => {
            tracing::info!("dev environment updated — restarting Tilt on the new version");
        }
        Ok(Update::PulledOnly) if dev.has_dev_shell() => {
            // Doesn't say *how*: we didn't start this Tilt, so we don't know
            // what did.
            tracing::info!("dev environment updated — restart Tilt to pick up the new dev shell");
            refresh_button(dev, false);
        }
        Ok(Update::PulledOnly) => {
            tracing::info!("dev environment updated — no dev shell here, so no restart needed");
            refresh_button(dev, false);
        }
        Err(e) => tracing::error!(error = %e, "updating the dev environment failed"),
    }
}

/// Re-checks the dev-env repo against its remote on the poll tick. Guarded so a
/// slow fetch can't overlap the next one.
pub fn poll(dev_env: Option<Arc<DevEnv>>, polling: Arc<AtomicBool>) {
    let Some(dev) = dev_env else {
        return;
    };
    if polling
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    tokio::task::spawn_blocking(move || {
        refresh_button(&dev, true);
        polling.store(false, Ordering::SeqCst);
    });
}
