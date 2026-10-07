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
    pub crawl_rate: f64,
}

pub async fn run(db: Db, args: Args) -> Result<()> {
    let labs_before = db.labs_count().await?;
    let n = db.requeue_done().await?;
    info!("refresh: {n} crawl cells re-queued");
    // Bearer > session jar > form login. The jar self-renews all
    // run long; the bearer dies with its 1h token; form login only
    // works from non-walled egress.
    let client = if !args.bearer.is_empty() {
        info!("refresh: using pre-minted bearer");
        Client::new(
            args.crawl_rate,
            std::sync::Arc::new(crate::auth::Auth::static_token(&args.bearer)),
        )?
    } else if !args.jar.is_empty() {
        info!("refresh: using browser session jar");
        Client::new(
            args.crawl_rate,
            std::sync::Arc::new(crate::auth::Auth::session(&args.jar).await?),
        )?
    } else {
        let auth =
            std::sync::Arc::new(crate::auth::Auth::login(&args.username, &args.password).await?);
        info!("refresh: logged in, starting crawl");
        Client::new(args.crawl_rate, auth)?
    };
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
