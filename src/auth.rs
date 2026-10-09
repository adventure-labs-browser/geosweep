//! geocaching.com authentication with transparent, multi-source recovery.
//!
//! The durable path is a browser helper backed by Playwright. Authentication
//! is not a one-time choice: on every renewal we try every configured source
//! in order until one works, then retry the whole chain with bounded backoff.
//! A stale browser session is rebuilt by the helper; a static bearer is only
//! a one-shot fallback and can never shadow a renewable source.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

const SIGNIN_URL: &str =
    "https://www.geocaching.com/account/signin?returnUrl=%2Fplay";
const TOKEN_URL: &str = "https://www.geocaching.com/account/oauth/token";
const EXPIRY_SKEW_SECS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Browser,
    Session,
    Static,
    Form,
}

pub(crate) fn configured_sources(
    helper: &str,
    jar: &str,
    bearer: &str,
    username: &str,
    password: &str,
) -> Vec<Source> {
    let mut out = Vec::new();
    if !helper.is_empty() {
        out.push(Source::Browser);
    }
    if !jar.is_empty() {
        out.push(Source::Session);
    }
    if !bearer.is_empty() {
        out.push(Source::Static);
    }
    if !username.is_empty() && !password.is_empty() {
        out.push(Source::Form);
    }
    out
}

struct AuthState {
    mode: Mode,
    token: String,
    expires_at: Instant,
}

struct ResilientSources {
    helper: Option<(String, String)>,
    session_http: Option<reqwest::Client>,
    static_token: Option<String>,
    form_http: Option<reqwest::Client>,
    username: String,
    password: String,
}

enum Mode {
    Resilient(ResilientSources),
    Form { username: String, password: String },
}

pub struct Auth {
    http: reqwest::Client,
    inner: tokio::sync::Mutex<AuthState>,
}

impl Auth {
    /// Convenience entry point for the browser-auth preflight. It still keeps
    /// every other configured source as a fallback.
    pub async fn browser(helper: &str, state_path: &str) -> Result<Self> {
        let jar = std::env::var("GC_JAR").unwrap_or_default();
        let bearer = std::env::var("GC_BEARER").unwrap_or_default();
        let username = std::env::var("GC_USER").unwrap_or_default();
        let password = std::env::var("GC_PASS").unwrap_or_default();
        Self::resilient(helper, state_path, &jar, &bearer, &username, &password).await
    }

    /// Multi-source auth. This is the mode used by crawl/refresh/verify.
    ///
    /// Renewal order is browser helper -> copied session jar -> static bearer
    /// -> direct form login. Failure of one source never selects it forever:
    /// every later renewal starts from the strongest source again.
    pub async fn resilient(
        helper: &str,
        state_path: &str,
        jar_path: &str,
        bearer: &str,
        username: &str,
        password: &str,
    ) -> Result<Self> {
        let configured = configured_sources(helper, jar_path, bearer, username, password);
        if configured.is_empty() {
            anyhow::bail!("no authentication sources configured");
        }
        info!("auth sources configured: {configured:?}");

        let session_http = if jar_path.is_empty() {
            None
        } else {
            match load_jar(jar_path).and_then(|jar| base_client(Some(jar))) {
                Ok(http) => Some(http),
                Err(e) => {
                    warn!("legacy session jar could not be loaded ({e}); continuing with fallbacks");
                    None
                }
            }
        };

        let form_http = if username.is_empty() || password.is_empty() {
            None
        } else {
            match base_client(None) {
                Ok(http) => Some(http),
                Err(e) => {
                    warn!("direct form-login client could not be built ({e}); continuing with fallbacks");
                    None
                }
            }
        };

        let mut sources = ResilientSources {
            helper: (!helper.is_empty()).then(|| (helper.to_string(), state_path.to_string())),
            session_http,
            static_token: (!bearer.is_empty()).then(|| bearer.to_string()),
            form_http,
            username: username.to_string(),
            password: password.to_string(),
        };

        let (token, expires_in) = renew_resilient(&mut sources).await?;
        Ok(Self::wrap(
            base_client(None)?,
            Mode::Resilient(sources),
            token,
            expires_in,
        ))
    }

    /// Full direct form login. Kept for the explicit `auth` command.
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

    fn wrap(http: reqwest::Client, mode: Mode, token: String, expires_in: u64) -> Self {
        Self {
            http,
            inner: tokio::sync::Mutex::new(AuthState {
                mode,
                token,
                expires_at: expiry_from(expires_in),
            }),
        }
    }

    /// Return a usable token, proactively renewing before expiry.
    ///
    /// The mutex stays held through renewal so all workers share one renewal
    /// instead of stampeding the login/token endpoints.
    pub async fn token(&self) -> Result<String> {
        let mut st = self.inner.lock().await;
        if Instant::now() < st.expires_at {
            return Ok(st.token.clone());
        }
        self.refresh_locked(&mut st).await
    }

    /// A request rejected this exact token. If another worker already replaced
    /// it, reuse that newer token; otherwise run the full recovery chain.
    pub async fn refresh_rejected(&self, rejected_token: &str) -> Result<String> {
        let mut st = self.inner.lock().await;
        if st.token != rejected_token {
            return Ok(st.token.clone());
        }
        self.refresh_locked(&mut st).await
    }

    /// Force the exact same renewal path used for expiry/401 recovery.
    /// Used by the workflow preflight so renewal is proven immediately,
    /// rather than waiting an hour to discover a broken recovery path.
    pub async fn force_refresh(&self) -> Result<()> {
        let mut st = self.inner.lock().await;
        self.refresh_locked(&mut st).await?;
        Ok(())
    }

    async fn refresh_locked(&self, st: &mut AuthState) -> Result<String> {
        let (token, expires_in) = match &mut st.mode {
            Mode::Resilient(sources) => renew_resilient(sources).await?,
            Mode::Form { username, password } => {
                form_login(&self.http, username, password).await?;
                mint(&self.http).await?
            }
        };
        st.token = token.clone();
        st.expires_at = expiry_from(expires_in);
        Ok(token)
    }
}

fn expiry_from(expires_in: u64) -> Instant {
    Instant::now()
        + Duration::from_secs(expires_in.saturating_sub(EXPIRY_SKEW_SECS).max(30))
}

async fn renew_resilient(sources: &mut ResilientSources) -> Result<(String, u64)> {
    let mut cycle: u32 = 0;

    loop {
        cycle += 1;

        if let Some((helper, state_path)) = &sources.helper {
            match browser_mint_once(helper, state_path).await {
                Ok((token, expires_in, source)) => {
                    info!("auth recovered via browser/{source}; token lifetime {expires_in}s");
                    return Ok((token, expires_in));
                }
                Err(e) => warn!("browser auth failed: {e}; trying fallback sources"),
            }
        }

        if let Some(http) = &sources.session_http {
            match mint(http).await {
                Ok((token, expires_in)) => {
                    info!("auth recovered via copied browser session");
                    return Ok((token, expires_in));
                }
                Err(e) => warn!("copied session auth failed: {e}; trying fallback sources"),
            }
        }

        // A supplied bearer is useful only once. If it later gets rejected or
        // expires, reusing the same bytes cannot possibly recover anything.
        if let Some(token) = sources.static_token.take() {
            info!("auth temporarily using one-shot static bearer fallback");
            return Ok((token, 3600));
        }

        if let Some(http) = &sources.form_http {
            match form_login(http, &sources.username, &sources.password).await {
                Ok(()) => match mint(http).await {
                    Ok((token, expires_in)) => {
                        info!("auth recovered via direct form login");
                        return Ok((token, expires_in));
                    }
                    Err(e) => warn!("direct form login succeeded but token mint failed: {e}"),
                },
                Err(e) => warn!("direct form login failed: {e}"),
            }
        }

        // When every credential source fails, ease off: repeatedly attempting
        // login dozens of times an hour will not fix an upstream outage and
        // may trigger additional account protection. Crawl-time cancellation
        // still interrupts this backoff without losing its page checkpoint.
        let wait = (1u64 << cycle.min(9)).min(600);
        warn!("all auth sources failed in cycle {cycle}; retrying entire chain in {wait}s");
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

async fn browser_mint_once(
    helper: &str,
    state_path: &str,
) -> Result<(String, u64, String)> {
    if !std::path::Path::new(helper).is_file() {
        anyhow::bail!("browser auth helper does not exist: {helper}");
    }

    let mut command = tokio::process::Command::new("python3");
    command
        .arg(helper)
        .arg("--state")
        .arg(state_path)
        .kill_on_drop(true);

    let out = match tokio::time::timeout(Duration::from_secs(240), command.output()).await {
        Ok(result) => result.context("start browser auth helper")?,
        Err(_) => anyhow::bail!("browser auth helper exceeded 240s and was killed"),
    };

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail: String = stderr.chars().take(500).collect();
        anyhow::bail!("helper status {}: {}", out.status, detail.trim());
    }

    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("browser auth helper JSON")?;
    let token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .filter(|t| t.matches('.').count() == 2)
        .context("browser auth helper returned invalid access_token")?
        .to_string();
    let expires_in = v
        .get("expires_in")
        .and_then(|e| e.as_u64())
        .unwrap_or(3600);
    let source = v
        .get("source")
        .and_then(|s| s.as_str())
        .unwrap_or("browser")
        .to_string();

    Ok((token, expires_in, source))
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
    use super::{configured_sources, Source};

    #[test]
    fn all_sources_are_kept_in_failover_order() {
        assert_eq!(
            configured_sources("helper.py", "jar.json", "token", "user", "pass"),
            vec![Source::Browser, Source::Session, Source::Static, Source::Form]
        );
    }

    #[test]
    fn missing_sources_are_skipped_not_selected_forever() {
        assert_eq!(
            configured_sources("helper.py", "", "", "user", "pass"),
            vec![Source::Browser, Source::Form]
        );
    }
}
