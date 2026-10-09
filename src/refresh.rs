//! Daily refresh pass: re-queue crawl cells and pick up new/changed
//! caches. Discovery inserts are idempotent; field changes append to
//! cache_versions. Every stage is resumable.

use anyhow::{Context, Result};
use std::time::Duration;
use tracing::info;

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
    // This happens before crawl::run starts its own runtime deadline. If the
    // OAuth endpoint is serving HTML instead of JWT JSON, an unbounded renewal
    // loop here could consume the whole job without starting any discovery.
    let auth = tokio::time::timeout(
        Duration::from_secs(180),
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
    .context("initial authentication unavailable for 180 seconds")??;
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
    // Avoid publishing an unchanged checkpoint if initial OAuth fails.
    if let Ok(marker) = std::env::var("GC_CRAWL_STARTED_MARKER") {
        std::fs::write(&marker, "started\n")
            .with_context(|| format!("write crawl-started marker {marker}"))?;
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
            max_runtime_secs: args.crawl_budget_secs,
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
