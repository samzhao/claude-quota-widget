mod credentials;
mod usage;

use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};
use usage::{UsageError, UsageWindow};

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum AccountStatus {
    Ok,
    NeedsLogin,
    RateLimited,
    Error,
}

/// Everything the frontend gets for one account. No token material.
#[derive(Serialize)]
struct AccountUsage {
    id: String,
    label: String,
    plan: Option<String>,
    read_only: bool,
    status: AccountStatus,
    message: Option<String>,
    windows: Vec<UsageWindow>,
    fetched_at_ms: f64,
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

#[tauri::command]
async fn get_default_account_usage() -> AccountUsage {
    let mut row = AccountUsage {
        id: "default".to_string(),
        label: credentials::read_default_email().unwrap_or_else(|| "~/.claude".to_string()),
        plan: None,
        read_only: true,
        status: AccountStatus::Ok,
        message: None,
        windows: Vec::new(),
        fetched_at_ms: now_ms(),
    };

    let Some(oauth) = credentials::read_default() else {
        row.status = AccountStatus::NeedsLogin;
        row.message = Some("No Claude Code login found on this Mac.".to_string());
        return row;
    };
    row.plan = oauth.subscription_type.clone();

    if oauth.is_expired(now_ms()) {
        row.status = AccountStatus::NeedsLogin;
        row.message = Some("Token expired. Run Claude Code on this Mac once to refresh it.".to_string());
        return row;
    }

    match usage::fetch(&oauth.access_token).await {
        Ok(windows) => row.windows = windows,
        Err(UsageError::Unauthorized) => {
            row.status = AccountStatus::NeedsLogin;
            row.message = Some("Token rejected. Run Claude Code on this Mac once to refresh it.".to_string());
        }
        Err(UsageError::RateLimited) => {
            row.status = AccountStatus::RateLimited;
            row.message = Some("Usage endpoint is throttling. Will retry.".to_string());
        }
        Err(UsageError::Other(message)) => {
            row.status = AccountStatus::Error;
            row.message = Some(message);
        }
    }
    row
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![get_default_account_usage])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
