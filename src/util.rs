//! Small shared helpers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tracing::warn;

/// Shared stop flag, set on SIGINT *or* SIGTERM (so `timeout`, `kill`,
/// tmux pane-close and Ctrl-C all trigger a clean stop).
pub fn shutdown_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        warn!("shutdown signal: finishing in-flight work then stopping");
        flag.store(true, Ordering::SeqCst);
    });
    stop
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
