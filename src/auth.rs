//! geocaching.com auth with transparent renewal.
//!
//! Authentication modes:
//!   1. Browser helper: reuses a saved Playwright session, falls back to a
//!      fresh browser login, and can repeat that cycle forever.
//!   2. Session cookies: legacy in-process cookie-jar renewal.
//!   3. Static bearer: explicit non-renewable fallback only.
//!   4. Form login: residential egress only; datacenter IPs are bot-walled.
//!
//! The GitHub workflow uses browser-helper mode so bearer expiry, cookie
//! expiry, and a stale saved browser session all recover without losing crawl
//! progress.
//!
//! Tokens and cookies live in memory (+ the ephemeral storage_state file
//! in the workspace, never uploaded). Nothing is written to the db.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

const SIGNIN_URL: &str =
    "https://www.geocaching.com/account/signin?returnUrl=%2Fplay";
const TOKEN_URL: &str = "https://www.geocaching.com/account/oauth/token";
/// Refresh ahead of the stated expiry by this much.
const EXPIRY_SKEW_SECS: u64 = 600;

struct AuthState {
    mode: Mode,
    token: String,
    expires_at: Instant,
}

enum Mode {
    Browser { helper: String, state_path: String },
    Form { username: String, password: String },
    Session,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Browser,
    Session,
    Static,
    Form,
}

/// Pick the strongest available auth source. Browser-helper mode is fully
/// self-healing and therefore wins over all pre-minted or copied auth state.
pub(crate) fn select_source(helper: &str, jar: &str, bearer: &str) -> Source {
    if !helper.is_empty() {
        Source::Browser
    } else if !jar.is_empty() {
        Source::Session
    } else if !bearer.is_empty() {
        Source::Static
    } else {
        Source::Form
    }
}

pub struct Auth {
    http: reqwest::Client,
    inner: tokio::sync::Mutex<AuthState>,
}

impl Auth {
    /// Durable runner mode. The helper owns browser state and can rebuild it
    /// from GC_USER/GC_PASS whenever the saved session no longer works.
    pub async fn browser(helper: &str, state_path: &str) -> Result<Self> {
        let http = base_client(None)?;
        let (token, expires_in) = browser_mint(helper, state_path).await?;
        Ok(Self::wrap(
            http,
            Mode::Browser {
                helper: helper.to_string(),
                state_path: state_path.to_string(),
            },
            token,
            expires_in,
        ))
    }

    /// Full form login (residential only).
    pub async fn login(username: &str, password: &str) -> Result<Self> {
        let http = base_client(None)?;
        form_login(&http, username, password).await?;
        let (token, expires_in) = mint(&http).await?;
        Ok(Self::wrap(
            http,
            Mode::Form {
                username: username.to_string(),
                password: password.to_string(),
            },
            token,
            expires_in,
        ))
    }

    /// Session-cookie mode: loads a browser storage_state file, mints the
    /// first bearer from it, renews the same way forever after.
    pub async fn session(jar_path: &str) -> Result<Self> {
        let http = base_client(Some(load_jar(jar_path)?))?;
        let (token, expires_in) = mint(&http).await?;
        Ok(Self::wrap(http, Mode::Session, token, expires_in))
    }

    /// Wrap an externally minted bearer. Expiry unknown (~1h); renewal
    /// is impossible and fails loudly instead of re-logging-in wrong.
    pub fn static_token(token: &str) -> Self {
        let http = base_client(None).expect("tls client builds");
        Self::wrap(http, Mode::Static, token.to_string(), 3600)
    }

    fn wrap(http: reqwest::Client, mode: Mode, token: String, expires_in: u64) -> Self {
        Self {
            http,
            inner: tokio::sync::Mutex::new(AuthState {
                mode,
                token,
                expires_at: Instant::now()
                    + Duration::from_secs(expires_in.saturating_sub(EXPIRY_SKEW_SECS)),
            }),
        }
    }

    /// Valid token, renewing first if stale. The mutex is held through
    /// renewal so sixteen workers cannot stampede the token endpoint.
    pub async fn token(&self) -> Result<String> {
        let mut st = self.inner.lock().await;
        if Instant::now() < st.expires_at {
            return Ok(st.token.clone());
        }
        self.refresh_locked(&mut st).await
    }

    /// Refresh after a 401, but only if the rejected token is still current.
    /// If another worker already renewed it, reuse that worker's token.
    pub async fn refresh_rejected(&self, rejected_token: &str) -> Result<String> {
        let mut st = self.inner.lock().await;
        if st.token != rejected_token {
            return Ok(st.token.clone());
        }
        self.refresh_locked(&mut st).await
    }

    async fn refresh_locked(&self, st: &mut AuthState) -> Result<String> {
        let (token, expires_in) = match &st.mode {
            Mode::Browser { helper, state_path } => {
                browser_mint(helper, state_path).await?
            }
            Mode::Form { username, password } => {
                form_login(&self.http, username, password).await?;
                mint(&self.http).await?
            }
            Mode::Session => mint(&self.http).await?,
            Mode::Static => {
                anyhow::bail!(
                    "static bearer expired mid-run; configure GC_AUTH_HELPER for self-healing auth"
                )
            }
        };
        st.token = token.clone();
        st.expires_at =
            Instant::now() + Duration::from_secs(expires_in.saturating_sub(EXPIRY_SKEW_SECS));
        Ok(token)
    }
}

async fn browser_mint(helper: &str, state_path: &str) -> Result<(String, u64)> {
    if !std::path::Path::new(helper).is_file() {
        anyhow::bail!("browser auth helper does not exist: {helper}");
    }

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let output = tokio::process::Command::new("python3")
            .arg(helper)
            .arg("--state")
            .arg(state_path)
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => {
                match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    Ok(v) => {
                        let token = v
                            .get("access_token")
                            .and_then(|t| t.as_str())
                            .map(str::to_string);
                        let expires_in = v
                            .get("expires_in")
                            .and_then(|e| e.as_u64())
                            .unwrap_or(3600);
                        let source = v
                            .get("source")
                            .and_then(|s| s.as_str())
                            .unwrap_or("browser");
                        if let Some(token) = token.filter(|t| t.matches('.').count() == 2) {
                            info!(
                                "browser auth ready via {source}; token lifetime {expires_in}s"
                            );
                            return Ok((token, expires_in));
                        }
                        warn!(
                            "browser auth attempt {attempt} returned an invalid token payload; retrying"
                        );
                    }
                    Err(e) => {
                        warn!(
                            "browser auth attempt {attempt} returned invalid JSON ({e}); retrying"
                        );
                    }
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let detail: String = stderr.chars().take(500).collect();
                warn!(
                    "browser auth attempt {attempt} failed (status {}): {}; retrying",
                    out.status,
                    detail.trim()
                );
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::bail!("python3 is required for browser authentication");
                }
                warn!("browser auth attempt {attempt} could not start ({e}); retrying");
            }
        }

        let shift = attempt.min(5);
        let wait = (1u64 << shift).min(60);
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

fn base_client(jar: Option<reqwest::cookie::Jar>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(crate::api::USER_AGENT);
    b = match jar {
        Some(j) => b.cookie_provider(Arc::new(j)),
        None => b.cookie_store(true),
    };
    Ok(b.build()?)
}

/// Load a Playwright storage_state file into a cookie jar.
fn load_jar(path: &str) -> Result<reqwest::cookie::Jar> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).context("read cookie jar")?)?;
    let jar = reqwest::cookie::Jar::default();
    let cookies = v
        .get("cookies")
        .and_then(|c| c.as_array())
        .context("storage_state has no cookies")?;
    for ck in cookies {
        let (Some(name), Some(value)) = (
            ck.get("name").and_then(|s| s.as_str()),
            ck.get("value").and_then(|s| s.as_str()),
        ) else {
            continue;
        };
        let domain = ck
            .get("domain")
            .and_then(|s| s.as_str())
            .unwrap_or(".geocaching.com");
        let url = format!("https://{}/", domain.trim_start_matches('.'))
            .parse()
            .context("cookie url")?;
        jar.add_cookie_str(&format!("{name}={value}"), &url);
    }
    Ok(jar)
}

/// Website form login into the client's cookie store/session.
async fn form_login(
    http: &reqwest::Client,
    username: &str,
    password: &str,
) -> Result<()> {
    let signin_resp = http.get(SIGNIN_URL).send().await.context("signin page fetch")?;
    let signin_status = signin_resp.status();
    let page = signin_resp.text().await.unwrap_or_default();
    if !signin_status.is_success() {
        anyhow::bail!(
            "signin page http {signin_status}: {}",
            page.chars().take(200).collect::<String>()
        );
    }
    let token = extract_token(&page)?;
    let login_resp = http
        .post(SIGNIN_URL)
        .form(&[
            ("UsernameOrEmail", username),
            ("Password", password),
            ("__RequestVerificationToken", &token),
        ])
        .send()
        .await
        .context("login post")?;
    let login_status = login_resp.status();
    let _ = login_resp.text().await.unwrap_or_default();
    if !login_status.is_success() {
        anyhow::bail!("login post http {login_status}");
    }
    Ok(())
}

/// Mint an api-proxy bearer from the client's session cookies.
async fn mint(http: &reqwest::Client) -> Result<(String, u64)> {
    let tok_resp = http.get(TOKEN_URL).send().await.context("oauth token fetch")?;
    let tok_status = tok_resp.status();
    let tok_body = tok_resp.text().await.unwrap_or_default();
    if !tok_status.is_success() {
        anyhow::bail!(
            "oauth token http {tok_status}: {}",
            tok_body.chars().take(200).collect::<String>()
        );
    }
    let tok: serde_json::Value =
        serde_json::from_str(&tok_body).context("oauth token json")?;
    let access = tok
        .get("access_token")
        .and_then(|t| t.as_str())
        .context("oauth response missing access_token")?
        .to_string();
    let expires_in = tok
        .get("expires_in")
        .and_then(|e| e.as_u64())
        .unwrap_or(3600);
    Ok((access, expires_in))
}

/// Extract __RequestVerificationToken from the signin page HTML.
fn extract_token(page: &str) -> Result<String> {
    let key = r#"name="__RequestVerificationToken""#;
    let i = page
        .find(key)
        .context("signin page has no verification token")?;
    let rest = &page[i..];
    let v = rest
        .find("value=\"")
        .context("token has no value")?;
    let start = v + "value=\"".len();
    let end = rest[start..]
        .find('"')
        .context("token value unterminated")?;
    Ok(rest[start..start + end].to_string())
}

#[cfg(test)]
mod tests {
    use super::{select_source, Source};

    #[test]
    fn browser_helper_beats_every_legacy_source() {
        assert_eq!(
            select_source("gc-login.py", "gc-storage.json", "stale-token"),
            Source::Browser
        );
    }

    #[test]
    fn session_jar_beats_static_bearer_without_helper() {
        assert_eq!(
            select_source("", "gc-storage.json", "stale-token"),
            Source::Session
        );
    }

    #[test]
    fn static_bearer_is_only_used_without_renewable_auth() {
        assert_eq!(select_source("", "", "token"), Source::Static);
        assert_eq!(select_source("", "", ""), Source::Form);
    }
}
