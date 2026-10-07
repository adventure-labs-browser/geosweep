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
    let n = db.requeue_done().await?;
    info!("refresh: {n} crawl cells re-queued");
    // Keep every configured source alive. Expiry/401 walks the same
    // browser -> copied session -> one-shot bearer -> form-login chain and
    // retries the entire chain instead of poisoning crawl cells.
    info!("refresh: using resilient multi-source authentication");
    let client = Client::new(
        args.crawl_rate,
        std::sync::Arc::new(
            crate::auth::Auth::resilient(
                &args.auth_helper,
                &args.browser_state,
                &args.jar,
                &args.bearer,
                &args.username,
                &args.password,
            )
            .await?,
        ),
    )?;
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
