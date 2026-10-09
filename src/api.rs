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
    (500..600).contains(&status)
}

/// Server throttling is not a failed page. Retry indefinitely with bounded
/// exponential backoff when Retry-After is absent. The crawler's runtime
/// budget cancels the request and releases its page checkpoint if necessary.
fn throttle_delay(retry_after: Option<f64>, throttles: u32) -> Duration {
    let backoff = BACKOFF.powi(throttles.min(10) as i32).max(1.0);
    Duration::from_secs_f64(retry_after.unwrap_or(backoff).max(1.0))
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

/// Adaptive limiter that learns a sustainable rate instead of oscillating.
///
/// learned_interval is the long-lived rate we believe the server accepts.
/// We spend almost all our time there. Occasionally, after a long clean run,
/// we probe 10% faster. A failed probe immediately returns to the last known
/// good rate; a 429 at the known-good rate makes that rate more conservative.
/// Concurrent 429s are coalesced for 30s so one burst cannot cascade to 30s.
struct Adaptive {
    interval: Duration,
    learned_interval: Duration,
    clean: u64,
    probing: bool,
    last_cut: Option<Instant>,
    last_probe: Option<Instant>,
}

const MIN_INTERVAL: Duration = Duration::from_millis(50); // 20/s hard ceiling
const MAX_INTERVAL: Duration = Duration::from_secs(30);
const CUT_COOLDOWN: Duration = Duration::from_secs(30);
const PROBE_COOLDOWN: Duration = Duration::from_secs(120);
// Shorter clean streaks speed up cautious recovery from old throttle events.
const PROBE_SUCCESSES: u64 = 40;
const TEMP_RECOVERY_SUCCESSES: u64 = 20;
const PROBE_FACTOR: f64 = 0.90;
// A lone 429 should not erase hours of proven successful throughput.
const CUT_FACTOR: f64 = 1.5;

impl Adaptive {
    fn with_learned_rate(rate: f64, learned_rate: Option<f64>) -> Self {
        let initial_rate = learned_rate
            .filter(|r| r.is_finite() && *r > 0.0)
            .unwrap_or(rate);
        let interval = if initial_rate > 0.0 {
            Duration::from_secs_f64(1.0 / initial_rate)
                .clamp(MIN_INTERVAL, MAX_INTERVAL)
        } else {
            Duration::ZERO
        };
        Self {
            interval,
            learned_interval: interval,
            clean: 0,
            probing: false,
            last_cut: None,
            last_probe: None,
        }
    }

    fn current(&self) -> Duration {
        self.interval
    }

    fn learned_rate(&self) -> f64 {
        if self.learned_interval.is_zero() {
            0.0
        } else {
            1.0 / self.learned_interval.as_secs_f64()
        }
    }

    /// A clean request normally changes nothing. We only move after a long
    /// stable streak: either shed a temporary Retry-After floor, validate a
    /// probe, or begin a small 10% probe above the learned rate.
    fn success(&mut self) -> Option<(&'static str, Duration, Duration)> {
        if self.interval.is_zero() {
            return None;
        }
        self.clean += 1;
        let old = self.interval;

        if self.interval > self.learned_interval && self.clean >= TEMP_RECOVERY_SUCCESSES {
            self.interval = self
                .interval
                .mul_f64(0.5)
                .max(self.learned_interval)
                .max(MIN_INTERVAL);
            self.clean = 0;
            return (self.interval < old)
                .then_some(("recovering temporary backoff", old, self.interval));
        }

        if self.probing && self.clean >= PROBE_SUCCESSES {
            self.learned_interval = self.interval;
            self.probing = false;
            self.clean = 0;
            self.last_probe = Some(Instant::now());
            return Some(("accepted faster learned rate", old, self.interval));
        }

        if !self.probing
            && self.interval == self.learned_interval
            && self.clean >= PROBE_SUCCESSES
            && self
                .last_cut
                .is_none_or(|t| t.elapsed() >= PROBE_COOLDOWN)
            && self
                .last_probe
                .is_none_or(|t| t.elapsed() >= PROBE_COOLDOWN)
            && self.interval > MIN_INTERVAL
        {
            self.interval = self.interval.mul_f64(PROBE_FACTOR).max(MIN_INTERVAL);
            self.probing = true;
            self.clean = 0;
            self.last_probe = Some(Instant::now());
            return Some(("probing faster rate", old, self.interval));
        }

        None
    }

    /// Handle a 429. A failed probe falls straight back to the previous
    /// learned-good rate. Otherwise, at most once per 30s, make the learned
    /// rate more conservative. Retry-After can temporarily force us slower
    /// without permanently poisoning the learned rate.
    fn cut(&mut self, floor: Duration) -> (Duration, bool, bool) {
        let now = Instant::now();
        let old = self.interval;
        let mut learned_changed = false;

        if self.probing {
            self.interval = self.learned_interval.max(floor).min(MAX_INTERVAL);
            self.probing = false;
            self.clean = 0;
            self.last_cut = Some(now);
            return (self.interval, self.interval > old, false);
        }

        let in_burst = self
            .last_cut
            .is_some_and(|last| now.saturating_duration_since(last) < CUT_COOLDOWN);

        if !in_burst {
            let candidate = self
                .learned_interval
                .mul_f64(CUT_FACTOR)
                .clamp(MIN_INTERVAL, MAX_INTERVAL);
            if candidate > self.learned_interval {
                self.learned_interval = candidate;
                learned_changed = true;
            }
            self.last_cut = Some(now);
        }

        self.interval = self
            .learned_interval
            .max(floor)
            .clamp(MIN_INTERVAL, MAX_INTERVAL);
        self.clean = 0;
        (self.interval, self.interval > old, learned_changed)
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
        Self::with_learned_rate(rate, None, auth)
    }

    pub fn with_learned_rate(
        rate: f64,
        learned_rate: Option<f64>,
        auth: Arc<Auth>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            http,
            auth,
            next_slot: Arc::new(Mutex::new(Instant::now())),
            adaptive: Arc::new(Mutex::new(Adaptive::with_learned_rate(
                rate,
                learned_rate,
            ))),
        })
    }

    pub fn learned_rate(&self) -> f64 {
        self.adaptive.lock().unwrap().learned_rate()
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

    /// Record a clean request. Most successes deliberately do not change
    /// the rate; that is what lets the limiter settle for long periods.
    fn note_success(&self) {
        if let Some((what, old, new)) = self.adaptive.lock().unwrap().success() {
            warn!(
                "rate limiter {what}: {:.3}/s -> {:.3}/s",
                1.0 / old.as_secs_f64(),
                1.0 / new.as_secs_f64()
            );
        }
    }

    /// Record a 429 and hold all future acquisitions behind the new slot.
    fn note_limited(&self, retry_after: Duration) {
        let (interval, changed, learned_changed) =
            self.adaptive.lock().unwrap().cut(retry_after);
        if changed || learned_changed {
            warn!(
                "rate limiter 429: settling at {:.3}/s (learned {:.3}/s)",
                1.0 / interval.as_secs_f64(),
                self.learned_rate()
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
        self.search_url(&url).await
    }

    async fn search_url(&self, url: &str) -> Result<SearchResp, ApiError> {
        let mut attempt: u32 = 0;
        let mut auth_failures: u32 = 0;
        let mut throttles: u32 = 0;

        loop {
            self.acquire().await;
            let token = self.auth_token().await?;
            let resp = match self
                .http
                .get(url)
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

            if status == 429 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|v| v.is_finite() && *v >= 0.0 && *v < 86_400.0);
                // A 429 is a pause, NEVER an exhausted-retries failure. The
                // same page stays claimed until it succeeds or the runtime
                // budget cancels the request and returns it to pending.
                // Only a real server Retry-After becomes a global floor.
                self.note_limited(
                    retry_after
                        .map(Duration::from_secs_f64)
                        .unwrap_or(Duration::ZERO),
                );
                throttles = throttles.saturating_add(1);
                let delay = throttle_delay(retry_after, throttles);
                if throttles == 1 || throttles.is_multiple_of(20) {
                    warn!("HTTP 429: retrying same page after {delay:?} (429 #{throttles}; never marking failed)");
                }
                tokio::time::sleep(delay).await;
                continue;
            }

            if retriable(status) {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|v| v.is_finite() && *v >= 0.0 && *v < 86_400.0);
                let wait = retry_after.unwrap_or_else(|| BACKOFF.powi(attempt as i32));
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
    fn throttling_is_not_an_exhaustible_failure() {
        assert!(!retriable(429));
        assert!(retriable(500));
        assert!(retriable(503));
        for attempt in 1..=100 {
            // Even after more than RETRIES 429s, no retry cap is involved.
            let wait = throttle_delay(None, attempt);
            assert!(wait >= Duration::from_secs(1));
            assert!(wait <= Duration::from_secs(202));
        }
        assert_eq!(throttle_delay(Some(25.0), 999), Duration::from_secs(25));
    }

    #[tokio::test]
    async fn eight_429s_then_200_preserves_the_same_page() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for n in 0..(RETRIES + 3) {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let bytes_read = socket.read(&mut request).unwrap();
                assert!(bytes_read > 0);
                let response = if n < RETRIES + 2 {
                    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                } else {
                    let body = r#"{"total":1,"results":[{"code":"GCTEST"}]}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                socket.write_all(response.as_bytes()).unwrap();
            }
        });

        let auth = Auth::resilient("", "", "", "test-token", "", "")
            .await
            .unwrap();
        let client = Client::new(0.0, Arc::new(auth)).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(30), client.search_url(&url))
            .await
            .expect("429s should not stall beyond bounded Retry-After")
            .expect("429 must never consume the normal retry limit");
        assert_eq!(result.total_count, 1);
        assert_eq!(result.items[0]["code"], "GCTEST");
        server.join().unwrap();
    }

    #[test]
    fn throttle_burst_only_changes_learned_rate_once() {
        let mut a = Adaptive::with_learned_rate(10.0, None);
        let (first, _, learned_changed) = a.cut(Duration::ZERO);
        assert!(learned_changed);
        assert_eq!(first, Duration::from_millis(150));
        assert!((a.learned_rate() - (10.0 / 1.5)).abs() < 0.001);

        let (second, _, learned_changed) = a.cut(Duration::ZERO);
        assert!(!learned_changed);
        assert_eq!(second, first);
        assert!((a.learned_rate() - (10.0 / 1.5)).abs() < 0.001);
    }

    #[test]
    fn retry_after_is_temporary_not_the_new_learned_rate() {
        let mut a = Adaptive::with_learned_rate(1.0, None);
        let (interval, _, learned_changed) = a.cut(Duration::from_secs(15));
        assert!(learned_changed);
        assert_eq!(interval, Duration::from_secs(15));
        assert_eq!(a.learned_interval, Duration::from_millis(1500));
        for _ in 0..TEMP_RECOVERY_SUCCESSES {
            a.success();
        }
        assert_eq!(a.current(), Duration::from_millis(7500));
        assert_eq!(a.learned_interval, Duration::from_millis(1500));
    }

    #[test]
    fn failed_probe_returns_to_last_good_rate() {
        let mut a = Adaptive::with_learned_rate(1.0, None);
        for _ in 0..PROBE_SUCCESSES {
            a.success();
        }
        assert!(a.probing);
        assert_eq!(a.current(), Duration::from_millis(900));

        let (after, _, learned_changed) = a.cut(Duration::ZERO);
        assert!(!learned_changed);
        assert!(!a.probing);
        assert_eq!(after, Duration::from_secs(1));
        assert!((a.learned_rate() - 1.0).abs() < 0.001);
    }

    #[test]
    fn clean_probe_becomes_new_learned_rate() {
        let mut a = Adaptive::with_learned_rate(1.0, None);
        for _ in 0..PROBE_SUCCESSES {
            a.success();
        }
        for _ in 0..PROBE_SUCCESSES {
            a.success();
        }
        assert!(!a.probing);
        assert!((a.learned_rate() - (1.0 / 0.9)).abs() < 0.001);
    }

    #[test]
    fn persisted_rate_is_used_on_start() {
        let a = Adaptive::with_learned_rate(10.0, Some(0.75));
        assert!((a.learned_rate() - 0.75).abs() < 0.001);
        assert_eq!(a.current(), a.learned_interval);
    }
}
