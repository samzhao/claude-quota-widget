//! Fetches subscription quota usage for one OAuth access token.
//!
//! The endpoint and beta header are undocumented Claude Code internals and can
//! change without notice, so they live only in this file.

use serde::Serialize;
use std::time::Duration;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Clone)]
pub struct UsageWindow {
    pub key: String,
    pub label: String,
    /// Percent used, 0-100.
    pub utilization: f64,
    /// ISO timestamp or null when the window has not started.
    pub resets_at: Option<String>,
}

pub enum UsageError {
    Unauthorized,
    RateLimited,
    Other(String),
}

fn label_for(key: &str) -> String {
    match key {
        "five_hour" => "5-hour".to_string(),
        "seven_day" => "7-day".to_string(),
        other => other
            .replace("seven_day", "7-day")
            .replace("five_hour", "5-hour")
            .split('_')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Any top-level object with a numeric `utilization` is a window. Parsing this
/// loosely means new per-model windows show up without a code change.
fn windows_from(body: &serde_json::Value) -> Vec<UsageWindow> {
    let Some(map) = body.as_object() else {
        return Vec::new();
    };
    let mut windows: Vec<UsageWindow> = map
        .iter()
        .filter_map(|(key, value)| {
            let utilization = value.get("utilization")?.as_f64()?;
            Some(UsageWindow {
                key: key.clone(),
                label: label_for(key),
                utilization,
                resets_at: value
                    .get("resets_at")
                    .and_then(|r| r.as_str())
                    .map(str::to_string),
            })
        })
        .collect();
    let rank = |key: &str| match key {
        "five_hour" => 0,
        "seven_day" => 1,
        _ => 2,
    };
    windows.sort_by(|a, b| rank(&a.key).cmp(&rank(&b.key)).then(a.key.cmp(&b.key)));
    windows
}

pub async fn fetch(access_token: &str) -> Result<Vec<UsageWindow>, UsageError> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("claude-quota-widget/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| UsageError::Other(e.to_string()))?;

    let response = client
        .get(USAGE_URL)
        .bearer_auth(access_token)
        .header("anthropic-beta", OAUTH_BETA)
        .send()
        .await
        .map_err(|e| UsageError::Other(e.without_url().to_string()))?;

    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(UsageError::Unauthorized),
        429 => return Err(UsageError::RateLimited),
        // Status only. The body is never surfaced in case it echoes request details.
        other => return Err(UsageError::Other(format!("usage endpoint returned {other}"))),
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| UsageError::Other(e.without_url().to_string()))?;
    Ok(windows_from(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_and_unknown_windows_and_skips_nulls() {
        let body = serde_json::json!({
            "seven_day_opus": { "utilization": 12.5, "resets_at": null },
            "seven_day": { "utilization": 40.0, "resets_at": "2026-09-22T12:00:00+00:00" },
            "five_hour": { "utilization": 1.0, "resets_at": "2026-09-19T12:30:00+00:00" },
            "seven_day_oauth_apps": null,
            "limits": [{ "kind": "x" }]
        });
        let windows = windows_from(&body);
        let keys: Vec<&str> = windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["five_hour", "seven_day", "seven_day_opus"]);
        assert_eq!(windows[2].label, "7-day Opus");
        assert_eq!(windows[2].resets_at, None);
    }

    /// Live check against the real endpoint with this Mac's default login.
    /// Run with: cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_default_account_usage() {
        let oauth = crate::credentials::read_default().expect("default credentials");
        let result = tauri::async_runtime::block_on(fetch(&oauth.access_token));
        let windows = match result {
            Ok(w) => w,
            Err(UsageError::Unauthorized) => panic!("unauthorized"),
            Err(UsageError::RateLimited) => panic!("rate limited"),
            Err(UsageError::Other(m)) => panic!("{m}"),
        };
        for w in &windows {
            println!("{}: {}% resets_at={:?}", w.label, w.utilization, w.resets_at);
        }
        assert!(windows.iter().any(|w| w.key == "five_hour"));
    }
}
