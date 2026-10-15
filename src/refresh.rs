//! Daily refresh pass: re-queue crawl cells and pick up new/changed
//! caches. Discovery inserts are idempotent; field changes append to
//! cache_versions. Every stage is resumable.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::{api::Client, crawl, db::Db};

pub struct Args {
    pub username: String,
    pub password: String,
    pub bearer: String,
    pub jar: String,
    pub auth_helper: String,
    pub browser_state: String,
    pub crawl_rate: f64,
    pub crawl_budget_secs: u64,
}

pub async fn run(db: Db, args: Args) -> Result<()> {
    // Login retries must be accounted for in the overall refresh time budget.
    let pass_started = Instant::now();
    let labs_before = db.labs_count().await?;
    let before = db.stats().await?;
    if before.pending > 0 || before.in_progress > 0 || before.failed > 0 {
        // A previous bounded run stopped before worldwide discovery finished.
        // Resume only unfinished work; re-queuing completed leaves here would
        // throw away most of the checkpointing benefit.
        let retried = db.reset_failed().await?;
        info!(
            "refresh: resuming incomplete discovery — {} pending, {} in_progress, {} failed re-queued; completed leaves stay done",
            before.pending,
            before.in_progress,
            retried
        );
    } else {
        let n = db.requeue_done().await?;
        info!("refresh: complete baseline — {n} crawl cells re-queued for refresh");
    }
    // Keep every configured credential source available. Renewal always
    // retries the complete chain instead of committing to whichever source
    // happened to work first.
    info!("refresh: using resilient authentication chain");
    // Intermittent site-side sign-in rejection needs more than two backoff
    // cycles, but a hard deadline still protects the checkpoint/publish window.
    let auth_timeout_secs = std::env::var("GC_INITIAL_AUTH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(2700);
    info!("refresh: initial authentication retry window {auth_timeout_secs}s");
    let auth = tokio::time::timeout(
        Duration::from_secs(auth_timeout_secs),
        crate::auth::Auth::resilient(
            &args.auth_helper,
            &args.browser_state,
            &args.jar,
            &args.bearer,
            &args.username,
            &args.password,
        ),
    )
    .await
    .with_context(|| format!("initial authentication unavailable for {auth_timeout_secs} seconds"))??;
    let auth = std::sync::Arc::new(auth);
    const RATE_KEY: &str = "adaptive_learned_rate_rps";
    let learned_rate = db
        .meta_get(RATE_KEY)
        .await?
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0);
    if let Some(rate) = learned_rate {
        info!("refresh: restoring learned API rate {rate:.3}/s from previous run");
    }

    let client = Client::with_learned_rate(args.crawl_rate, learned_rate, auth)?;
    // Like LabSweep, obtain the entire searchable population from one
    // planet-sized query with take=1. Do this right after our existing login,
    // before the long-running crawl, so discovery need not finish for a count.
    // Count checks must never block or invalidate an otherwise healthy crawl.
    match tokio::time::timeout(Duration::from_secs(45), client.global_total()).await {
        Ok(Ok(total)) if total > 0 => {
            db.meta_set("worldwide_search_total", &total.to_string()).await?;
            info!("refresh: worldwide searchable geocaches = {total}");
        }
        Ok(Ok(_)) => warn!("refresh: worldwide search returned zero; ignoring suspect count"),
        Ok(Err(err)) => warn!("refresh: worldwide search count unavailable: {err}"),
        Err(_) => warn!("refresh: worldwide search count timed out; continuing crawl"),
    }
    // Avoid publishing an unchanged checkpoint if initial OAuth fails.
    if let Ok(marker) = std::env::var("GC_CRAWL_STARTED_MARKER") {
        std::fs::write(&marker, "started\n")
            .with_context(|| format!("write crawl-started marker {marker}"))?;
    }
    let remaining_budget = remaining_crawl_budget(args.crawl_budget_secs, pass_started.elapsed());
    if args.crawl_budget_secs > 0 {
        info!("refresh: {remaining_budget}s of overall pass budget remain for discovery");
    }
    let crawl_result = crawl::run(
        db.clone(),
        client.clone(),
        crawl::Args {
            concurrency: 16,
            seeds: 16,
            seed_radius_m: 12_000_000.0,
            min_radius_m: 1000.0,
            max_cells: 0,
            reset: false,
            reset_failed: false,
            max_runtime_secs: remaining_budget,
        },
    )
    .await;

    let learned_rate = client.learned_rate();
    db.meta_set(RATE_KEY, &format!("{learned_rate:.6}")).await?;
    info!("refresh: saved learned API rate {learned_rate:.3}/s for next run");
    crawl_result?;
    let labs_after = db.labs_count().await?;
    info!(
        "refresh: +{} new caches this run ({labs_before} -> {labs_after})",
        labs_after.saturating_sub(labs_before),
    );
    info!("refresh: done — {}", db.stats().await?);
    Ok(())
}

/// Include authentication delay in the budget so temporary upstream sign-in
/// failures do not cause the GitHub Actions hard timeout to discard progress.
fn remaining_crawl_budget(configured: u64, elapsed: Duration) -> u64 {
    if configured == 0 {
        0
    } else {
        configured.saturating_sub(elapsed.as_secs()).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::remaining_crawl_budget;
    use std::time::Duration;

    #[test]
    fn auth_delay_reduces_discovery_budget() {
        assert_eq!(remaining_crawl_budget(17_100, Duration::from_secs(2_700)), 14_400);
        assert_eq!(remaining_crawl_budget(17_100, Duration::ZERO), 17_100);
    }

    #[test]
    fn unlimited_and_exhausted_budgets() {
        assert_eq!(remaining_crawl_budget(0, Duration::from_secs(30_000)), 0);
        assert_eq!(remaining_crawl_budget(100, Duration::from_secs(150)), 1);
    }
}
