//! geocaching.com website auth: form login -> session cookies ->
//! OAuth bearer for the api-proxy. Tokens live in memory ONLY (never
//! written to the db), so there is nothing to strip before upload and
//! every run logs in fresh from env credentials.

use anyhow::{Context, Result};

const SIGNIN_URL: &str =
    "https://www.geocaching.com/account/signin?returnUrl=%2Fplay";
const TOKEN_URL: &str = "https://www.geocaching.com/account/oauth/token";

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

/// Username + password -> api-proxy bearer token. Builds its own
/// cookie-enabled client (the website session lives in cookies).
pub async fn login(username: &str, password: &str) -> Result<String> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
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
    let tok: serde_json::Value = serde_json::from_str(&tok_body)
        .with_context(|| {
            format!(
                "oauth token json (login post was {login_status}, {} bytes): {}",
                login_body.len(),
                tok_body.chars().take(200).collect::<String>()
            )
        })?;
    let access = tok
        .get("access_token")
        .and_then(|t| t.as_str())
        .context("oauth response missing access_token")?;
    Ok(access.to_string())
}
