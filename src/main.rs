mod api;
mod auth;
mod crawl;
mod db;
mod geo;
mod refresh;
mod util;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "geosweep", version, about = "Traditional geocache location mirror")]
struct Cli {
    /// SQLite database path.
    #[arg(long, global = true, default_value = "data/geosweep.db")]
    db: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Stage 1: quad-tree box discovery of every geocache.
    Crawl {
        /// Concurrent workers.
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
        /// Max aggregate requests/sec across all workers (0 = unlimited).
        #[arg(long, default_value_t = 10.0)]
        rate: f64,
        /// Geocaching username (env: GC_USER).
        #[arg(long, env = "GC_USER")]
        username: String,
        /// Geocaching password (env: GC_PASS).
        #[arg(long, env = "GC_PASS")]
        password: String,
        /// Number of fibonacci-sphere seed cells.
        #[arg(long, default_value_t = 16)]
        seeds: usize,
        /// Seed cell radius in meters.
        #[arg(long, default_value_t = 12_000_000.0)]
        seed_radius_m: f64,
        /// Below this radius a cell accepts partial results instead of splitting.
        #[arg(long, default_value_t = 1000.0)]
        min_radius_m: f64,
        /// Stop after N processed cells (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        max_cells: usize,
        /// Wipe the queue + discovered caches and start over.
        #[arg(long)]
        reset: bool,
        /// Re-queue cells marked failed.
        #[arg(long)]
        reset_failed: bool,
    },
    /// Refresh pass: re-queue cells, pick up new/changed caches.
    Refresh {
        /// Geocaching username (env: GC_USER).
        #[arg(long, env = "GC_USER")]
        username: String,
        /// Geocaching password (env: GC_PASS).
        #[arg(long, env = "GC_PASS")]
        password: String,
        /// Max aggregate requests/sec.
        #[arg(long, default_value_t = 20.0)]
        crawl_rate: f64,
    },
    /// Print queue/cache counts.
    Stats,
    /// Compare local cache count against the API's global totalCount.
    Verify {
        /// Geocaching username (env: GC_USER).
        #[arg(long, env = "GC_USER")]
        username: String,
        /// Geocaching password (env: GC_PASS).
        #[arg(long, env = "GC_PASS")]
        password: String,
    },
    /// Test website credentials (logs in, prints result).
    Auth {
        /// Geocaching username (env: GC_USER).
        #[arg(long, env = "GC_USER")]
        username: String,
        /// Geocaching password (env: GC_PASS).
        #[arg(long, env = "GC_PASS")]
        password: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let db = db::Db::open(&cli.db)?;

    async fn authed(rate: f64, username: &str, password: &str) -> Result<api::Client> {
        let bearer = auth::login(username, password).await?;
        println!("logged in");
        Ok(api::Client::new(rate, bearer)?)
    }

    match cli.cmd {
        Cmd::Crawl {
            concurrency,
            rate,
            username,
            password,
            seeds,
            seed_radius_m,
            min_radius_m,
            max_cells,
            reset,
            reset_failed,
        } => {
            let client = authed(rate, &username, &password).await?;
            crawl::run(
                db,
                client,
                crawl::Args {
                    concurrency,
                    seeds,
                    seed_radius_m,
                    min_radius_m,
                    max_cells,
                    reset,
                    reset_failed,
                },
            )
            .await
        }
        Cmd::Refresh {
            username,
            password,
            crawl_rate,
        } => {
            refresh::run(
                db,
                refresh::Args {
                    username,
                    password,
                    crawl_rate,
                },
            )
            .await
        }
        Cmd::Stats => {
            println!("{}", db.stats().await?);
            Ok(())
        }
        Cmd::Verify { username, password } => {
            let client = authed(5.0, &username, &password).await?;
            crawl::verify_global(&db, &client).await;
            Ok(())
        }
        Cmd::Auth { username, password } => {
            auth::login(&username, &password).await?;
            println!("login ok (token in memory only, nothing stored)");
            Ok(())
        }
    }
}
