//! geocaching.com website auth: form login -> session cookies ->
//! OAuth bearer for the api-proxy, with transparent refresh.
//!
//! The website token lives ~1h; a full crawl runs for hours, so the
//! client re-logs-in proactively before expiry and reactively on 401.
//! Tokens live in memory ONLY (never written to the db).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const SIGNIN_URL: &str =
    "https://www.geocaching.com/account/signin?returnUrl=%2Fplay";
const TOKEN_URL: &str = "https://www.geocaching.com/account/oauth/token";
/// Refresh ahead of the stated expiry by this much.
const EXPIRY_SKEW_SECS: u64 = 300;

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

struct AuthState {
    mode: Mode,
    token: String,
    expires_at: Instant,
}

enum Mode {
    /// Website credentials: can re-login any time.
    Login { username: String, password: String },
    /// Pre-minted bearer (e.g. via browser login step): cannot renew.
    Static,
}

pub struct Auth {
    inner: tokio::sync::Mutex<AuthState>,
}

impl Auth {
    pub async fn login(username: &str, password: &str) -> Result<Self> {
        let (token, expires_in) = do_login(username, password).await?;
        Ok(Self::from_parts(
            Mode::Login {
                username: username.to_string(),
                password: password.to_string(),
            },
            token,
            expires_in,
        ))
    }

    /// Wrap an externally minted bearer (browser login step). Expiry is
    /// unknown; assume one hour from now. Renewal is impossible — a
    /// stale static bearer fails loudly instead of re-logging-in wrong.
    pub fn static_token(token: &str) -> Self {
        Self::from_parts(Mode::Static, token.to_string(), 3600)
    }

    fn from_parts(mode: Mode, token: String, expires_in: u64) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(AuthState {
                mode,
                token,
                expires_at: Instant::now()
                    + Duration::from_secs(expires_in.saturating_sub(EXPIRY_SKEW_SECS)),
            }),
        }
    }

    /// Valid token, refreshing first if stale.
    pub async fn token(&self) -> Result<String> {
        {
            let st = self.inner.lock().await;
            if Instant::now() < st.expires_at {
                return Ok(st.token.clone());
            }
        }
        self.refresh().await
    }

    /// Unconditional re-login (after a 401). Returns the new token.
    pub async fn force_refresh(&self) -> Result<String> {
        self.refresh().await
    }

    async fn refresh(&self) -> Result<String> {
        let mode = {
            let st = self.inner.lock().await;
            match &st.mode {
                Mode::Login { username, password } => {
                    (username.clone(), password.clone())
                }
                Mode::Static => {
                    anyhow::bail!(
                        "static bearer expired mid-run; re-mint it in the login step"
                    )
                }
            }
        };
        let (u, p) = mode;
        let (token, expires_in) = do_login(&u, &p).await?;
        let mut st = self.inner.lock().await;
        st.token = token.clone();
        st.expires_at =
            Instant::now() + Duration::from_secs(expires_in.saturating_sub(EXPIRY_SKEW_SECS));
        Ok(token)
    }
}

/// Full login flow. Returns (bearer token, expires_in seconds).
async fn do_login(username: &str, password: &str) -> Result<(String, u64)> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(crate::api::USER_AGENT)
        .cookie_store(true)
        .build()?;
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
    let login_body = login_resp.text().await.unwrap_or_default();
    // A 200 can still be a bot-check or failed-login page: the proof is
    // whether the token endpoint then yields a JWT.
    let tok_resp = http.get(TOKEN_URL).send().await.context("oauth token fetch")?;
    let tok_status = tok_resp.status();
    let tok_body = tok_resp.text().await.unwrap_or_default();
    if !tok_status.is_success() {
        anyhow::bail!(
            "oauth token http {tok_status} (login post was {login_status}, {} bytes): {}",
            login_body.len(),
            tok_body.chars().take(200).collect::<String>()
        );
    }
    let tok: serde_json::Value = serde_json::from_str(&tok_body).with_context(|| {
        format!(
            "oauth token json (login post was {login_status}, {} bytes): {}",
            login_body.len(),
            tok_body.chars().take(200).collect::<String>()
        )
    })?;
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
