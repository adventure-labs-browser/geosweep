//! geocaching.com website search API client (`/api/proxy/web/search/v2`).
//!
//! Reverse-engineered from c:geo + live probes (2026-10-06):
//!   GET /api/proxy/web/search/v2?box=LATMAX,LONMIN,LATMIN,LONMAX
//!       &take=N&skip=M[&sort=distance&asc=true]
//!     Bearer: OAuth token from the website session (see auth.rs).
//!   - Box-based: boxes tile exactly, no circular gaps.
//!   - take maxes at 1000 (asked 2000, got 1000); we use 500.
//!   - Window rule: skip+take past ~10000 answers HTTP 500. Cells are
//!     fully paginable up to totalCount ~9500-10000; above that we split.
//!   - Basic accounts: premium-only caches come back WITHOUT coordinates
//!     (collected as coord-less teaser rows, ready to backfill). Premium
//!     accounts get everything with coords.
//!
//! One client is shared by all workers; a global fixed-interval limiter
//! caps the aggregate request rate.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tracing::warn;

use crate::geo::Cell;

pub const SEARCH_URL: &str = "https://www.geocaching.com/api/proxy/web/search/v2";
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

/// Page size. Verified accepted; the server caps at 1000.
pub const TAKE: usize = 500;
/// Above this totalCount a cell subdivides instead of paginating.
/// Conservative under the ~10000 window rule.
pub const PAGINATION_LIMIT: i64 = 9500;
/// Never request a page starting past this (window would end past 10000).
pub const MAX_SKIP: usize = 9500;

const RETRIES: u32 = 6;
const BACKOFF: f64 = 1.7;
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum ApiError {
    /// Permanent HTTP failure (non-retriable status).
    Status(u16, String),
    /// All retry attempts exhausted.
    Retries(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Status(c, s) => write!(f, "http {c}: {s}"),
            ApiError::Retries(m) => write!(f, "max retries exceeded: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}

pub struct SearchResp {
    pub total_count: u64,
    pub items: Vec<Value>,
}

fn retriable(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

fn snippet(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Bounding box for a radius cell: center ± radius, in degrees.
/// Over-approximates the disc (27% extra area); dedup absorbs overlap.
pub fn cell_box(cell: &Cell) -> (f64, f64, f64, f64) {
    let r_km = cell.radius / 1000.0;
    let dlat = r_km / 110.574;
    let cos_lat = cell.lat.to_radians().cos().max(0.0872);
    let dlon = r_km / (111.320 * cos_lat);
    (
        (cell.lat + dlat).min(90.0),
        (cell.lon - dlon).max(-180.0),
        (cell.lat - dlat).max(-90.0),
        (cell.lon + dlon).min(180.0),
    )
}

/// Adaptive rate limiter: additive-increase / multiplicative-decrease
/// on top of a fixed-interval scheduler. Clean requests gradually
/// tighten the interval (faster); any 429/5xx/timeout doubles it and
/// honors Retry-After. Converges to just under the server's patience
/// instead of needing a hand-picked rate.
struct Adaptive {
    interval: Duration,
    since_cut: u64,
}

const MIN_INTERVAL: Duration = Duration::from_millis(10); // 100/s hard ceiling
const MAX_INTERVAL: Duration = Duration::from_secs(30);
const SUCCESSES_PER_STEP: u64 = 50;

impl Adaptive {
    fn new(rate: f64) -> Self {
        Self {
            interval: if rate > 0.0 {
                Duration::from_secs_f64(1.0 / rate)
            } else {
                Duration::ZERO
            },
            since_cut: 0,
        }
    }

    fn current(&self) -> Duration {
        self.interval
    }

    /// Call after a clean request: every SUCCESSES_PER_STEP successes
    /// shaves a millisecond (additive increase toward the ceiling).
    fn success(&mut self) {
        if self.interval.is_zero() {
            return;
        }
        self.since_cut += 1;
        if self.since_cut >= SUCCESSES_PER_STEP && self.interval > MIN_INTERVAL {
            self.interval = (self.interval - Duration::from_millis(1)).max(MIN_INTERVAL);
            self.since_cut = 0;
        }
    }

    /// Call on 429/5xx/timeout: double the interval (floor Retry-After).
    fn cut(&mut self, floor: Duration) {
        let doubled = self.interval.mul_f64(2.0).max(floor);
        self.interval = doubled.min(MAX_INTERVAL);
        self.since_cut = 0;
        if self.interval >= Duration::from_secs(1) {
            warn!(
                "rate limiter backing off to {:.1}/s after throttling",
                1.0 / self.interval.as_secs_f64()
            );
        }
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    bearer: Arc<Mutex<Option<String>>>,
    next_slot: Arc<Mutex<Instant>>,
    adaptive: Arc<Mutex<Adaptive>>,
}

impl Client {
    pub fn new(rate: f64, bearer: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            http,
            bearer: Arc::new(Mutex::new(Some(bearer))),
            next_slot: Arc::new(Mutex::new(Instant::now())),
            adaptive: Arc::new(Mutex::new(Adaptive::new(rate))),
        })
    }

    async fn acquire(&self) {
        let step = self.adaptive.lock().unwrap().current();
        if step.is_zero() {
            return;
        }
        let wait = {
            let mut next = self.next_slot.lock().unwrap();
            let now = Instant::now();
            let slot = (*next).max(now);
            *next = slot + step;
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Record a clean request (tightens the schedule).
    fn note_success(&self) {
        self.adaptive.lock().unwrap().success();
    }

    /// Record throttling: double the interval (floor = Retry-After).
    fn note_limited(&self, retry_after: Duration) {
        self.adaptive.lock().unwrap().cut(retry_after);
    }

    fn auth_header(&self) -> String {
        format!(
            "Bearer {}",
            self.bearer.lock().unwrap().clone().unwrap_or_default()
        )
    }

    /// One box page. Retries retriable statuses with backoff.
    pub async fn search(
        &self,
        cell: &Cell,
        take: usize,
        skip: usize,
    ) -> Result<SearchResp, ApiError> {
        let (lat_max, lon_min, lat_min, lon_max) = cell_box(cell);
        let url = format!(
            "{SEARCH_URL}?box={lat_max},{lon_min},{lat_min},{lon_max}\
             &rad=16000&take={take}&skip={skip}&sort=distance&asc=true&app=geosweep"
        );
        let mut last_err = String::new();
        for attempt in 0..RETRIES {
            self.acquire().await;
            let resp = match self
                .http
                .get(&url)
                .header("Authorization", self.auth_header())
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("network: {e}");
                    self.note_limited(Duration::ZERO);
                    tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32))).await;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if status == 200 {
                match resp.json::<Value>().await {
                    Ok(v) => {
                        self.note_success();
                        return Ok(SearchResp {
                            total_count: v.get("total").and_then(|t| t.as_u64()).unwrap_or(0),
                            items: v
                                .get("results")
                                .and_then(|i| i.as_array())
                                .cloned()
                                .unwrap_or_default(),
                        });
                    }
                    Err(e) => {
                        last_err = format!("json: {e}");
                        tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32)))
                            .await;
                        continue;
                    }
                }
            }
            if status == 401 || status == 403 {
                let msg = snippet(&resp.text().await.unwrap_or_default(), 200);
                return Err(ApiError::Status(status, msg));
            }
            if retriable(status) {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or_else(|| BACKOFF.powi(attempt as i32));
                self.note_limited(Duration::from_secs_f64(wait));
                last_err = format!("http {status}: {}", snippet(&resp.text().await.unwrap_or_default(), 200));
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }
            return Err(ApiError::Status(status, snippet(&resp.text().await.unwrap_or_default(), 300)));
        }
        Err(ApiError::Retries(last_err))
    }

    /// Coverage check: totalCount for a planet-sized box. The API may
    /// refuse absurd boxes; failure is non-fatal (warned by caller).
    pub async fn global_total(&self) -> Result<u64, ApiError> {
        let url = format!(
            "{SEARCH_URL}?box=90,-180,-90,180&rad=16000&take=1&skip=0&app=geosweep"
        );
        self.acquire().await;
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ApiError::Retries(format!("network: {e}")))?;
        if resp.status().as_u16() != 200 {
            return Err(ApiError::Status(
                resp.status().as_u16(),
                snippet(&resp.text().await.unwrap_or_default(), 200),
            ));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Retries(format!("json: {e}")))?;
        Ok(v.get("total").and_then(|t| t.as_u64()).unwrap_or(0))
    }
}

