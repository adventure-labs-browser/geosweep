//! Daily refresh pass: re-queue crawl cells and pick up new/changed
//! caches. Discovery inserts are idempotent; field changes append to
//! cache_versions. Every stage is resumable.

use anyhow::Result;
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
            "refresh: resuming incomplete discovery — {} pending, {} in_progress,              {} failed re-queued; completed leaves stay done",
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
    let auth = std::sync::Arc::new(
        crate::auth::Auth::resilient(
            &args.auth_helper,
            &args.browser_state,
            &args.jar,
            &args.bearer,
            &args.username,
            &args.password,
        )
        .await?,
    );
    let client = Client::new(args.crawl_rate, auth)?;
    crawl::run(
        db.clone(),
        client,
        crawl::Args {
            concurrency: 16,
            seeds: 16,
            seed_radius_m: 12_000_000.0,
            min_radius_m: 1000.0,
            max_cells: 0,
            reset: false,
            reset_failed: false,
        },
    )
    .await?;
    let labs_after = db.labs_count().await?;
    info!(
        "refresh: +{} new caches this run ({labs_before} -> {labs_after})",
        labs_after.saturating_sub(labs_before),
    );
    info!("refresh: done — {}", db.stats().await?);
    Ok(())
}
