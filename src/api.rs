//! geocaching.com website search API client (`/api/proxy/web/search/v2`).
//!
//! Reverse-engineered from c:geo + live probes (2026-10-06):
//!   GET /api/proxy/web/search/v2?box=LATMAX,LONMIN,LATMIN,LONMAX
//!       &take=N&skip=M[&sort=distance&asc=true]
//!     Bearer: OAuth token from the website session (see auth.rs).
//!   - Box-based: boxes tile exactly, no circular gaps.
//!   - take maxes at 1000 (asked 2000, got 1000); we use the full 1000.
//!   - Window rule: skip+take past ~10000 answers HTTP 500. Cells are
//!     fully paginable up to totalCount ~9500-10000; above that we split.
//!   - Basic accounts: premium-only caches come back WITHOUT coordinates
//!     (collected as coord-less teaser rows, ready to backfill). Premium
//!     accounts get everything with coords.
//!
//! One client is shared by all workers; a global adaptive limiter
//! caps aggregate rate, coalesces throttle bursts, and rapidly recovers.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tracing::warn;

use crate::auth::Auth;
use crate::geo::Cell;

pub const SEARCH_URL: &str = "https://www.geocaching.com/api/proxy/web/search/v2";
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

/// Page size. Verified accepted; the server caps at 1000.
pub const TAKE: usize = 1000;
/// Above this totalCount a cell subdivides instead of paginating.
/// Conservative under the ~10000 window rule.
pub const PAGINATION_LIMIT: i64 = 9500;
/// Never request a page starting past this (window would end past 10000).
pub const MAX_SKIP: usize = 9000;

const RETRIES: u32 = 6;
const BACKOFF: f64 = 1.7;
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum ApiError {
    /// Authentication cannot continue without intervention.
    Auth(String),
    /// Permanent HTTP failure (non-retriable status).
    Status(u16, String),
    /// All retry attempts exhausted.
    Retries(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Auth(m) => write!(f, "authentication failed: {m}"),
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

/// Adaptive rate limiter with fast recovery and burst-coalesced backoff.
///
/// A single 429 burst can hit many concurrent workers. Treating every response
/// as an independent signal used to multiply the interval all the way to 30s.
/// We now cut at most once per short burst, honor Retry-After, and recover
/// exponentially after a handful of clean requests.
struct Adaptive {
    interval: Duration,
    base_interval: Duration,
    clean: u64,
    last_cut: Option<Instant>,
}

const MIN_INTERVAL: Duration = Duration::from_millis(50); // 20/s hard ceiling
const MAX_INTERVAL: Duration = Duration::from_secs(30);
const RECOVERY_SUCCESSES: u64 = 5;
const GROWTH_SUCCESSES: u64 = 50;
const CUT_COOLDOWN: Duration = Duration::from_secs(5);

impl Adaptive {
    fn new(rate: f64) -> Self {
        let interval = if rate > 0.0 {
            Duration::from_secs_f64(1.0 / rate)
        } else {
            Duration::ZERO
        };
        Self {
            interval,
            base_interval: interval,
            clean: 0,
            last_cut: None,
        }
    }

    fn current(&self) -> Duration {
        self.interval
    }

    /// Recover very quickly from throttle backoff, then cautiously probe
    /// above the configured starting rate.
    fn success(&mut self) -> Option<(Duration, Duration)> {
        if self.interval.is_zero() {
            return None;
        }
        self.clean += 1;
        let old = self.interval;
        if self.interval > self.base_interval && self.clean >= RECOVERY_SUCCESSES {
            self.interval = self
                .interval
                .mul_f64(0.5)
                .max(self.base_interval)
                .max(MIN_INTERVAL);
            self.clean = 0;
        } else if self.interval <= self.base_interval
            && self.clean >= GROWTH_SUCCESSES
            && self.interval > MIN_INTERVAL
        {
            self.interval = self.interval.mul_f64(0.9).max(MIN_INTERVAL);
            self.clean = 0;
        }
        (self.interval < old).then_some((old, self.interval))
    }

    /// Back off once per throttle burst instead of once per worker.
    /// Retry-After is always honored even when the multiplicative cut is
    /// coalesced with a recent one.
    fn cut(&mut self, floor: Duration) -> (Duration, bool) {
        let now = Instant::now();
        let burst = self
            .last_cut
            .is_some_and(|last| now.saturating_duration_since(last) < CUT_COOLDOWN);
        let old = self.interval;
        let candidate = if burst {
            old.max(floor)
        } else {
            old.mul_f64(2.0).max(floor)
        };
        self.interval = candidate.min(MAX_INTERVAL);
        self.clean = 0;
        if !burst {
            self.last_cut = Some(now);
        }
        (self.interval, self.interval > old)
    }
}
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    auth: Arc<Auth>,
    next_slot: Arc<Mutex<Instant>>,
    adaptive: Arc<Mutex<Adaptive>>,
}

impl Client {
    pub fn new(rate: f64, auth: Arc<Auth>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            http,
            auth,
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

    /// Record a clean request and rapidly recover from prior throttling.
    fn note_success(&self) {
        if let Some((old, new)) = self.adaptive.lock().unwrap().success() {
            if old >= Duration::from_secs(1) || new >= Duration::from_secs(1) {
                warn!(
                    "rate limiter recovering: one request every {:.2}s -> {:.2}s",
                    old.as_secs_f64(),
                    new.as_secs_f64()
                );
            }
        }
    }

    /// Record a 429. Concurrent responses from the same burst are coalesced,
    /// and new acquisitions are held behind the resulting quiet period.
    fn note_limited(&self, retry_after: Duration) {
        let (interval, changed) = self.adaptive.lock().unwrap().cut(retry_after);
        if changed {
            warn!(
                "rate limiter backing off to {:.2}/s (one request every {:.2}s)",
                1.0 / interval.as_secs_f64(),
                interval.as_secs_f64()
            );
        }
        let until = Instant::now() + interval.max(retry_after);
        let mut next = self.next_slot.lock().unwrap();
        if *next < until {
            *next = until;
        }
    }

    async fn auth_token(&self) -> Result<String, ApiError> {
        self.auth
            .token()
            .await
            .map_err(|e| ApiError::Auth(format!("token refresh: {e}")))
    }

    /// One box page. Network/server failures are bounded per cell, but
    /// authentication failures are not converted into failed cells: recover
    /// auth and retry the exact request until a credential source works.
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
        let mut attempt: u32 = 0;
        let mut auth_failures: u32 = 0;

        loop {
            self.acquire().await;
            let token = self.auth_token().await?;
            let resp = match self
                .http
                .get(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let message = format!("network: {e}");
                    // A transport failure already gets per-request exponential
                    // backoff. Do not globally poison throughput for unrelated
                    // DNS/TLS/network hiccups.
                    attempt += 1;
                    if attempt >= RETRIES {
                        return Err(ApiError::Retries(message));
                    }
                    tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32)))
                        .await;
                    continue;
                }
            };

            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                let detail = snippet(&resp.text().await.unwrap_or_default(), 160);
                auth_failures += 1;
                warn!(
                    "auth rejected with http {status} (attempt {auth_failures}): {detail}; rebuilding auth"
                );
                self.auth
                    .refresh_rejected(&token)
                    .await
                    .map_err(|e| ApiError::Auth(format!("token renewal after {status}: {e}")))?;
                attempt = 0;
                let wait = (1u64 << auth_failures.min(5)).min(60);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }

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
                        let message = format!("json: {e}");
                        attempt += 1;
                        if attempt >= RETRIES {
                            return Err(ApiError::Retries(message));
                        }
                        tokio::time::sleep(Duration::from_secs_f64(BACKOFF.powi(attempt as i32)))
                            .await;
                        continue;
                    }
                }
            }

            if retriable(status) {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or_else(|| BACKOFF.powi(attempt as i32));
                if status == 429 {
                    self.note_limited(Duration::from_secs_f64(wait));
                }
                let message =
                    format!("http {status}: {}", snippet(&resp.text().await.unwrap_or_default(), 200));
                attempt += 1;
                if attempt >= RETRIES {
                    return Err(ApiError::Retries(message));
                }
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }

            return Err(ApiError::Status(
                status,
                snippet(&resp.text().await.unwrap_or_default(), 300),
            ));
        }
    }

    /// Coverage check: totalCount for a planet-sized box. Authentication
    /// rejection uses the same self-healing path as ordinary search traffic.
    pub async fn global_total(&self) -> Result<u64, ApiError> {
        let url = format!(
            "{SEARCH_URL}?box=90,-180,-90,180&rad=16000&take=1&skip=0&app=geosweep"
        );
        let mut auth_failures: u32 = 0;

        loop {
            self.acquire().await;
            let token = self.auth_token().await?;
            let resp = self
                .http
                .get(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|e| ApiError::Retries(format!("network: {e}")))?;

            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                let detail = snippet(&resp.text().await.unwrap_or_default(), 160);
                auth_failures += 1;
                warn!(
                    "verify auth rejected with http {status} (attempt {auth_failures}): {detail}; rebuilding auth"
                );
                self.auth
                    .refresh_rejected(&token)
                    .await
                    .map_err(|e| ApiError::Auth(format!("token renewal after {status}: {e}")))?;
                let wait = (1u64 << auth_failures.min(5)).min(60);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }

            if status != 200 {
                return Err(ApiError::Status(
                    status,
                    snippet(&resp.text().await.unwrap_or_default(), 200),
                ));
            }

            let v: Value = resp
                .json()
                .await
                .map_err(|e| ApiError::Retries(format!("json: {e}")))?;
            return Ok(v.get("total").and_then(|t| t.as_u64()).unwrap_or(0));
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_burst_is_coalesced() {
        let mut a = Adaptive::new(5.0);
        let (first, changed) = a.cut(Duration::from_secs(1));
        assert!(changed);
        assert_eq!(first, Duration::from_secs(1));

        let (second, changed) = a.cut(Duration::ZERO);
        assert!(!changed);
        assert_eq!(second, first);

        // A server-provided floor still wins inside the burst window.
        let (third, changed) = a.cut(Duration::from_secs(3));
        assert!(changed);
        assert_eq!(third, Duration::from_secs(3));
    }

    #[test]
    fn throttle_recovery_is_exponential() {
        let mut a = Adaptive::new(5.0);
        let (interval, _) = a.cut(Duration::from_secs(30));
        assert_eq!(interval, Duration::from_secs(30));

        for _ in 0..RECOVERY_SUCCESSES {
            a.success();
        }
        assert_eq!(a.current(), Duration::from_secs(15));

        for _ in 0..RECOVERY_SUCCESSES {
            a.success();
        }
        assert_eq!(a.current(), Duration::from_millis(7500));

        // A few more clean batches get back near the configured rate,
        // rather than requiring millions of successes.
        for _ in 0..(RECOVERY_SUCCESSES * 7) {
            a.success();
        }
        assert_eq!(a.current(), Duration::from_millis(200));
    }
}
