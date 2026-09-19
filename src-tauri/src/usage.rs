//! Fetches subscription quota usage for one OAuth access token.
//!
//! The endpoint and beta header are undocumented Claude Code internals and can
//! change without notice, so they live only in this file.

use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct UsageWindow {
    /// Stable across accounts, so the grid view can line windows up in columns.
    pub key: String,
    pub label: String,
    /// Percent used, 0-100.
    pub utilization: f64,
    /// ISO timestamp or null when the window has not started.
    pub resets_at: Option<String>,
    /// Server's own read of the level: normal / warning / critical.
    pub severity: Option<String>,
}

pub enum UsageError {
    Unauthorized,
    RateLimited,
    Other(String),
}

fn title_case(snake: &str) -> String {
    snake
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
        .join(" ")
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The `limits` array is what the Claude apps render: one entry per limit,
/// including model-scoped ones like Fable that have no top-level field.
fn windows_from_limits(limits: &[Value]) -> Vec<UsageWindow> {
    limits
        .iter()
        .filter_map(|limit| {
            let kind = limit.get("kind")?.as_str()?;
            let utilization = limit.get("percent")?.as_f64()?;
            let model = limit
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str);
            let (key, label) = match (kind, model) {
                ("session", _) => ("session".to_string(), "5-hour".to_string()),
                ("weekly_all", _) => ("weekly_all".to_string(), "All models".to_string()),
                (_, Some(model)) => (format!("{kind}:{}", model.to_lowercase()), model.to_string()),
                (other, None) => (other.to_string(), title_case(other)),
            };
            Some(UsageWindow {
                key,
                label,
                utilization,
                resets_at: string_at(limit, "resets_at"),
                severity: string_at(limit, "severity"),
            })
        })
        .collect()
}

/// Older response shape: top-level `five_hour` / `seven_day` objects. Only the
/// two well-known ones, because the rest are unlabeled internal codenames.
fn windows_from_legacy(body: &Value) -> Vec<UsageWindow> {
    [("five_hour", "session", "5-hour"), ("seven_day", "weekly_all", "All models")]
        .into_iter()
        .filter_map(|(field, key, label)| {
            let window = body.get(field)?;
            Some(UsageWindow {
                key: key.to_string(),
                label: label.to_string(),
                utilization: window.get("utilization")?.as_f64()?,
                resets_at: string_at(window, "resets_at"),
                severity: None,
            })
        })
        .collect()
}

fn windows_from(body: &Value) -> Vec<UsageWindow> {
    let mut windows = body
        .get("limits")
        .and_then(Value::as_array)
        .map(|limits| windows_from_limits(limits))
        .filter(|windows| !windows.is_empty())
        .unwrap_or_else(|| windows_from_legacy(body));
    let rank = |key: &str| match key {
        "session" => 0,
        "weekly_all" => 1,
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

    let body: Value = response
        .json()
        .await
        .map_err(|e| UsageError::Other(e.without_url().to_string()))?;
    Ok(windows_from(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_array_wins_and_surfaces_model_scoped_limits() {
        let body = serde_json::json!({
            "five_hour": { "utilization": 4.0, "resets_at": "2026-09-19T12:30:00+00:00" },
            "nimbus_quill": { "utilization": 0.0, "resets_at": null },
            "limits": [
                { "kind": "weekly_scoped", "percent": 93, "severity": "critical",
                  "resets_at": "2026-09-22T22:00:00+00:00",
                  "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null } },
                { "kind": "weekly_all", "percent": 78, "severity": "warning",
                  "resets_at": "2026-09-22T22:00:00+00:00", "scope": null },
                { "kind": "session", "percent": 4, "severity": "normal",
                  "resets_at": "2026-09-19T12:30:00+00:00", "scope": null }
            ]
        });
        let windows = windows_from(&body);
        let summary: Vec<(&str, &str, f64)> = windows
            .iter()
            .map(|w| (w.key.as_str(), w.label.as_str(), w.utilization))
            .collect();
        assert_eq!(
            summary,
            [
                ("session", "5-hour", 4.0),
                ("weekly_all", "All models", 78.0),
                ("weekly_scoped:fable", "Fable", 93.0),
            ]
        );
        assert_eq!(windows[2].severity.as_deref(), Some("critical"));
    }

    #[test]
    fn falls_back_to_legacy_fields_without_codename_noise() {
        let body = serde_json::json!({
            "seven_day": { "utilization": 40.0, "resets_at": null },
            "five_hour": { "utilization": 1.0, "resets_at": "2026-09-19T12:30:00+00:00" },
            "nimbus_quill": { "utilization": 0.0, "resets_at": null },
            "seven_day_opus": null,
            "limits": []
        });
        let keys: Vec<String> = windows_from(&body).into_iter().map(|w| w.key).collect();
        assert_eq!(keys, ["session", "weekly_all"]);
    }

    /// Live check against the real endpoint with this Mac's default login.
    /// Run with: cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_default_account_usage() {
        let oauth = crate::credentials::read_default().expect("default credentials");
        let result = tauri::async_runtime::block_on(fetch(oauth.access_token()));
        let windows = match result {
            Ok(w) => w,
            Err(UsageError::Unauthorized) => panic!("unauthorized"),
            Err(UsageError::RateLimited) => panic!("rate limited"),
            Err(UsageError::Other(m)) => panic!("{m}"),
        };
        for w in &windows {
            println!("{}: {}% {:?} resets_at={:?}", w.label, w.utilization, w.severity, w.resets_at);
        }
        assert!(windows.iter().any(|w| w.key == "session"));
    }
}
