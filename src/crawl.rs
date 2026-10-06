//! Stage 1 — quad-tree discovery of every traditional geocache.
//!
//! Same design as labsweep's crawl, adapted to box queries: a cell whose
//! totalCount exceeds PAGINATION_LIMIT subdivides (boxes tile exactly,
//! so unlike circular queries there are no geometric gaps); smaller
//! cells paginate fully (every total <= limit is reachable — the last
//! page is skip=9000+take=500, window ending at the ~10000 cliff).
//! The SQLite queue makes everything resumable at page granularity.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{error, info, warn};

use crate::api::{Client, MAX_SKIP, PAGINATION_LIMIT, TAKE};
use crate::db::{ClaimedCell, Db};
use crate::geo::{self, Cell};

const STALE_CELL_SECS: i64 = 600;

pub struct Args {
    pub concurrency: usize,
    pub rate: f64,
    pub seeds: usize,
    pub seed_radius_m: f64,
    pub min_radius_m: f64,
    /// Stop after N cells (0 = unlimited). Dry-run escape hatch.
    pub max_cells: usize,
    pub reset: bool,
    pub reset_failed: bool,
}

pub async fn run(db: Db, client: Client, args: Args) -> Result<()> {
    if args.reset {
        db.reset().await?;
        info!("queue + caches wiped");
    }
    if args.reset_failed {
        let n = db.reset_failed().await?;
        info!("{n} failed cells re-queued (keeping page checkpoints)");
    }
    let reaped = db.reap_stale(STALE_CELL_SECS).await?;
    if reaped > 0 {
        warn!("reaped {reaped} stale in_progress cells (checkpoints kept)");
    }
    if db.queue_is_empty().await? {
        let seeds: Vec<Cell> = geo::fibonacci_sphere(args.seeds)
            .into_iter()
            .map(|(lat, lon)| Cell::new(lat, lon, args.seed_radius_m))
            .collect();
        db.seed(&seeds).await?;
        info!(
            "seeded {} cells at radius {}m",
            seeds.len(),
            args.seed_radius_m
        );
    }
    info!("start: {}", db.stats().await?);

    let client = Arc::new(client);
    let stop = crate::util::shutdown_flag();
    let processed = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::with_capacity(args.concurrency);
    for _ in 0..args.concurrency {
        let (db, client, stop, processed) =
            (db.clone(), client.clone(), stop.clone(), processed.clone());
        let (min_radius, max_cells) = (args.min_radius_m, args.max_cells);
        handles.push(tokio::spawn(async move {
            worker(db, client, stop, processed, min_radius, max_cells).await
        }));
    }
    for h in handles {
        h.await??;
    }
    info!("done: {}", db.stats().await?);
    verify_global(&db, &client).await;
    Ok(())
}

/// Coverage check with components: api_total vs labs vs known_dead.
/// `est_missing` near zero with plausible dead = probably fine.
pub async fn verify_global(db: &Db, client: &Client) {
    match client.global_total().await {
        Ok(api_total) => match db.coverage_counts().await {
            Ok((total, dead)) => {
                let live = total.saturating_sub(dead);
                let est_missing = api_total as i64 - live as i64;
                info!(
                    "verify: api_total={api_total} caches={total} \
                     known_dead={dead} live_est={live} est_missing={est_missing}"
                );
                if est_missing.abs() <= 500 {
                    info!(
                        "verify: probably fine \
                         (est missing {est_missing} of {api_total})"
                    );
                } else {
                    warn!(
                        "VERIFY MISMATCH — api {api_total}, live est {live} \
                         (caches {total} - dead {dead}): est missing {est_missing}"
                    );
                }
            }
            Err(e) => warn!("verify: coverage counts failed: {e}"),
        },
        Err(e) => warn!("verify: global query failed: {e}"),
    }
}

async fn worker(
    db: Db,
    client: Arc<Client>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    processed: Arc<AtomicU64>,
    min_radius_m: f64,
    max_cells: usize,
) -> Result<()> {
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        if max_cells > 0 && processed.load(Ordering::SeqCst) >= max_cells as u64 {
            return Ok(());
        }
        let Some(claimed) = claim_or_wait(&db, &stop).await? else {
            return Ok(());
        };
        process_cell(&db, &client, claimed, min_radius_m, &stop).await;
        processed.fetch_add(1, Ordering::SeqCst);
    }
}

/// `claim` returns None the moment `pending` is empty, but a sibling worker
/// may still be in-flight and about to subdivide. Wait until in_progress
/// also drains before declaring the queue exhausted.
async fn claim_or_wait(
    db: &Db,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<Option<ClaimedCell>> {
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if let Some(c) = db.claim().await? {
            return Ok(Some(c));
        }
        if db.in_progress_count().await? == 0 {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn process_cell(
    db: &Db,
    client: &Client,
    claimed: ClaimedCell,
    min_radius_m: f64,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) {
    let cell = &claimed.cell;
    let cid = cell.id.clone();
    let mut skip = claimed.next_skip.max(0) as usize;
    let total: u64;

    if skip == 0 {
        // Fresh cell — page 0 decides whether we paginate or subdivide.
        match client.search(cell, TAKE, 0).await {
            Ok(first) => {
                total = first.total_count;
                if let Err(e) = db
                    .record_page(cid.clone(), TAKE as i64, Some(total as i64), first.items)
                    .await
                {
                    error!("[{cid}] db error saving page 0: {e}");
                    return;
                }
                skip = TAKE;
            }
            Err(e) => {
                let _ = db.mark_failed(cid.clone(), e.to_string()).await;
                warn!("[{cid}] FAILED: {e}");
                return;
            }
        }
        if total as i64 > PAGINATION_LIMIT && cell.radius / 2.0 >= min_radius_m {
            let children = geo::subdivide(cell);
            let n = children.len();
            match db.subdivide(cid.clone(), children).await {
                Ok(()) => info!(
                    "[{cid}] SPLIT -> {n} children at radius {:.0}m \
                     (total={total} > {PAGINATION_LIMIT})",
                    cell.radius / 2.0
                ),
                Err(e) => error!("[{cid}] db error on subdivide: {e}"),
            }
            return;
        }
    } else {
        total = claimed.total_count.unwrap_or(0) as u64;
        info!("[{cid}] RESUME at skip={skip} total={total}");
    }

    // Paginate while pages are full and inside the window. Min-radius
    // overflow (total > limit, can't subdivide) ends at the HTTP 500
    // past the window and lands in mark_failed — none observed so far.
    let normal = total as i64 <= PAGINATION_LIMIT;
    while skip <= MAX_SKIP && (!normal || (skip as u64) < total) {
        if stop.load(Ordering::SeqCst) {
            let _ = db.release_claim(cid.clone()).await;
            info!("[{cid}] released at skip={skip} — resumes here next run");
            return;
        }
        match client.search(cell, TAKE, skip).await {
            Ok(page) => {
                let n = page.items.len();
                if let Err(e) = db
                    .record_page(
                        cid.clone(),
                        (skip + TAKE) as i64,
                        Some(total as i64),
                        page.items,
                    )
                    .await
                {
                    error!("[{cid}] db error saving page skip={skip}: {e}");
                    return;
                }
                if n < TAKE {
                    break;
                }
            }
            Err(e) => {
                let _ = db
                    .mark_failed(cid.clone(), format!("page skip={skip}: {e}"))
                    .await;
                warn!(
                    "[{cid}] FAILED at skip={skip} (pages saved; \
                     --reset-failed resumes here): {e}"
                );
                return;
            }
        }
        skip += TAKE;
    }

    match db.mark_done(cid.clone(), Some(total as i64)).await {
        Ok(()) if normal => info!("[{cid}] DONE total={total}"),
        Ok(()) => info!("[{cid}] MIN-RADIUS PARTIAL total={total} reached_skip={skip}"),
        Err(e) => error!("[{cid}] db error on mark_done: {e}"),
    }
}
