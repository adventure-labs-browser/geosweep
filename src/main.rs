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
        #[arg(long, env = "GC_USER", default_value = "")]
        username: String,
        /// Geocaching password (env: GC_PASS).
        #[arg(long, env = "GC_PASS", default_value = "")]
        password: String,
        /// Pre-minted bearer (env: GC_BEARER). Skips form login;
        /// use when datacenter egress is bot-walled.
        #[arg(long, env = "GC_BEARER", default_value = "")]
        bearer: String,
        /// Legacy browser session jar (env: GC_JAR).
        #[arg(long, env = "GC_JAR", default_value = "")]
        jar: String,
        /// Browser auth helper (env: GC_AUTH_HELPER). Re-authenticates as needed.
        #[arg(long, env = "GC_AUTH_HELPER", default_value = "")]
        auth_helper: String,
        /// Playwright storage-state path used by the browser auth helper.
        #[arg(long, env = "GC_BROWSER_STATE", default_value = "gc-storage.json")]
        browser_state: String,
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
        #[arg(long, env = "GC_USER", default_value = "")]
        username: String,
        /// Geocaching password (env: GC_PASS).
        #[arg(long, env = "GC_PASS", default_value = "")]
        password: String,
        /// Pre-minted bearer (env: GC_BEARER). Skips form login.
        #[arg(long, env = "GC_BEARER", default_value = "")]
        bearer: String,
        /// Legacy browser session jar (env: GC_JAR).
        #[arg(long, env = "GC_JAR", default_value = "")]
        jar: String,
        /// Browser auth helper (env: GC_AUTH_HELPER). Re-authenticates as needed.
        #[arg(long, env = "GC_AUTH_HELPER", default_value = "")]
        auth_helper: String,
        /// Playwright storage-state path used by the browser auth helper.
        #[arg(long, env = "GC_BROWSER_STATE", default_value = "gc-storage.json")]
        browser_state: String,
        /// Max aggregate requests/sec (adaptive limiter starts here
        /// and finds the ceiling on its own).
        #[arg(long, default_value_t = 5.0)]
        crawl_rate: f64,
    },
    /// Print queue/cache counts.
    Stats,
    /// Compare local cache count against the API's global totalCount.
    Verify {
        /// Geocaching username (env: GC_USER).
        #[arg(long, env = "GC_USER", default_value = "")]
        username: String,
        /// Geocaching password (env: GC_PASS).
        #[arg(long, env = "GC_PASS", default_value = "")]
        password: String,
        /// Pre-minted bearer (env: GC_BEARER).
        #[arg(long, env = "GC_BEARER", default_value = "")]
        bearer: String,
        /// Legacy browser session jar (env: GC_JAR).
        #[arg(long, env = "GC_JAR", default_value = "")]
        jar: String,
        /// Browser auth helper (env: GC_AUTH_HELPER).
        #[arg(long, env = "GC_AUTH_HELPER", default_value = "")]
        auth_helper: String,
        /// Playwright storage-state path used by the browser auth helper.
        #[arg(long, env = "GC_BROWSER_STATE", default_value = "gc-storage.json")]
        browser_state: String,
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

    async fn authed(
        rate: f64,
        username: &str,
        password: &str,
        bearer: &str,
        jar: &str,
        auth_helper: &str,
        browser_state: &str,
    ) -> Result<api::Client> {
        let auth = match auth::select_source(auth_helper, jar, bearer) {
            auth::Source::Browser => {
                println!("using self-healing browser authentication");
                std::sync::Arc::new(auth::Auth::browser(auth_helper, browser_state).await?)
            }
            auth::Source::Session => {
                println!("using legacy browser session jar");
                std::sync::Arc::new(auth::Auth::session(jar).await?)
            }
            auth::Source::Static => {
                println!("using pre-minted bearer (non-renewable)");
                std::sync::Arc::new(auth::Auth::static_token(bearer))
            }
            auth::Source::Form => {
                println!("logging in");
                std::sync::Arc::new(auth::Auth::login(username, password).await?)
            }
        };
        api::Client::new(rate, auth)
    }

    match cli.cmd {
        Cmd::Crawl {
            concurrency,
            rate,
            username,
            password,
            bearer,
            jar,
            auth_helper,
            browser_state,
            seeds,
            seed_radius_m,
            min_radius_m,
            max_cells,
            reset,
            reset_failed,
        } => {
            let client = authed(
                rate,
                &username,
                &password,
                &bearer,
                &jar,
                &auth_helper,
                &browser_state,
            )
            .await?;
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
            bearer,
            jar,
            auth_helper,
            browser_state,
            crawl_rate,
        } => {
            refresh::run(
                db,
                refresh::Args {
                    username,
                    password,
                    bearer,
                    jar,
                    auth_helper,
                    browser_state,
                    crawl_rate,
                },
            )
            .await
        }
        Cmd::Stats => {
            println!("{}", db.stats().await?);
            Ok(())
        }
        Cmd::Verify {
            username,
            password,
            bearer,
            jar,
            auth_helper,
            browser_state,
        } => {
            let client = authed(
                5.0,
                &username,
                &password,
                &bearer,
                &jar,
                &auth_helper,
                &browser_state,
            )
            .await?;
            crawl::verify_global(&db, &client).await;
            Ok(())
        }
        Cmd::Auth { username, password } => {
            auth::Auth::login(&username, &password).await?;
            println!("login ok (token in memory only, nothing stored)");
            Ok(())
        }
    }
}
