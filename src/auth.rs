//! geocaching.com auth with transparent renewal.
//!
//! Three modes:
//!   1. Session cookies (browser storage_state). Mints fresh bearers via
//!      the OAuth token endpoint all run long — the durable runner mode.
//!   2. Static bearer, only as an explicit fallback when no session jar is
//!      available. It dies with the token (~1h) and cannot renew.
//!   3. Form login (residential egress only; datacenter IPs are
//!      bot-walled at the signin page).
//!
//! Tokens and cookies live in memory (+ the ephemeral storage_state file
//! in the workspace, never uploaded). Nothing is written to the db.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const SIGNIN_URL: &str =
    "https://www.geocaching.com/account/signin?returnUrl=%2Fplay";
const TOKEN_URL: &str = "https://www.geocaching.com/account/oauth/token";
/// Refresh ahead of the stated expiry by this much.
const EXPIRY_SKEW_SECS: u64 = 300;

struct AuthState {
    mode: Mode,
    token: String,
    expires_at: Instant,
}

enum Mode {
    Form { username: String, password: String },
    Session,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Session,
    Static,
    Form,
}

/// Session auth must win whenever a jar is present. This intentionally
/// ignores an inherited GC_BEARER so a short-lived token cannot shadow
/// renewable browser-session auth.
pub(crate) fn select_source(jar: &str, bearer: &str) -> Source {
    if !jar.is_empty() {
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
        let credentials = match &st.mode {
            Mode::Form { username, password } => {
                Some((username.clone(), password.clone()))
            }
            Mode::Session => None,
            Mode::Static => {
                anyhow::bail!(
                    "static bearer expired mid-run; use a renewable session jar"
                )
            }
        };
        if let Some((u, p)) = credentials {
            form_login(&self.http, &u, &p).await?;
        }
        let (token, expires_in) = mint(&self.http).await?;
        st.token = token.clone();
        st.expires_at =
            Instant::now() + Duration::from_secs(expires_in.saturating_sub(EXPIRY_SKEW_SECS));
        Ok(token)
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
    fn session_jar_always_beats_static_bearer() {
        assert_eq!(select_source("gc-storage.json", "stale-token"), Source::Session);
    }

    #[test]
    fn static_bearer_is_only_used_without_a_jar() {
        assert_eq!(select_source("", "token"), Source::Static);
        assert_eq!(select_source("", ""), Source::Form);
    }
}
