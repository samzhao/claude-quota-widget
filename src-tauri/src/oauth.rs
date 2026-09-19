//! Refreshes access tokens for accounts this app owns.
//!
//! Only ever called for managed accounts. The default `~/.claude` login belongs
//! to Claude Code: refreshing it here would rotate its token out from under it.
//!
//! The token endpoint and client id are the public Claude Code ones. They are
//! undocumented internals and can change, so they live only in this file.

use crate::credentials::Oauth;
use serde_json::Value;
use std::time::Duration;

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const TIMEOUT: Duration = Duration::from_secs(15);
/// Refresh this long before expiry so a check never races the deadline.
pub const EXPIRY_BUFFER_MS: f64 = 5.0 * 60_000.0;

#[derive(Debug)]
pub enum RefreshError {
    /// The refresh token itself was rejected. Only a new login fixes this.
    Dead,
    /// Network trouble, throttling, server error. The stored login is untouched.
    Transient(String),
}

pub async fn refresh(oauth: &Oauth, now_ms: f64) -> Result<Oauth, RefreshError> {
    let refresh_token = oauth.refresh_token().ok_or(RefreshError::Dead)?;

    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("claude-quota-widget/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| RefreshError::Transient(e.to_string()))?;

    // Same shape the `claude` CLI sends: a form post with the public client id.
    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.without_url().to_string()))?;

    // Status only, never the body: error bodies can echo token material.
    match response.status().as_u16() {
        200 => {}
        400 | 401 | 403 => return Err(RefreshError::Dead),
        other => return Err(RefreshError::Transient(format!("token endpoint returned {other}"))),
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| RefreshError::Transient(e.without_url().to_string()))?;
    oauth
        .with_refresh_response(&body, now_ms)
        .ok_or_else(|| RefreshError::Transient("token endpoint sent no access token".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials;

    /// Spends one real token renewal on the managed account named by
    /// CQW_TEST_ACCOUNT_ID and stores the result, exactly as the app would.
    /// Run with: CQW_TEST_ACCOUNT_ID=<id> cargo test live_renew -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_renew_one_managed_account() {
        let id = std::env::var("CQW_TEST_ACCOUNT_ID").expect("CQW_TEST_ACCOUNT_ID");
        let before = credentials::read_managed(&id).expect("stored login");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;

        let renewed = tauri::async_runtime::block_on(refresh(&before, now)).expect("renew");
        credentials::write_managed(&id, &renewed).expect("store renewed login");

        let stored = credentials::read_managed(&id).expect("read back");
        assert_eq!(stored.access_token(), renewed.access_token());
        assert_ne!(stored.access_token(), before.access_token());
        assert!(stored.expires_at() > before.expires_at());
        println!(
            "renewed: refresh token rotated = {}, valid for {:.1} h",
            stored.refresh_token() != before.refresh_token(),
            (stored.expires_at().unwrap() - now) / 3_600_000.0
        );
    }
}
