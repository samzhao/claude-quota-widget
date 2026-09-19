mod accounts;
mod credentials;
mod keychain;
mod login;
mod usage;

use accounts::Account;
use credentials::Oauth;
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, State};
use tokio::sync::oneshot;
use usage::{UsageError, UsageWindow};

const DEFAULT_ID: &str = "default";

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

/// Holds the cancel handle of the one login allowed to run at a time.
#[derive(Default)]
struct LoginState(Mutex<Option<oneshot::Sender<()>>>);

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

async fn usage_row(id: String, label: String, read_only: bool, oauth: Option<Oauth>) -> AccountUsage {
    let relogin_hint = if read_only {
        "Run Claude Code on this Mac once to refresh it."
    } else {
        "Remove and re-add this account."
    };
    let mut row = AccountUsage {
        id,
        label,
        plan: None,
        read_only,
        status: AccountStatus::Ok,
        message: None,
        windows: Vec::new(),
        fetched_at_ms: now_ms(),
    };

    let Some(oauth) = oauth else {
        row.status = AccountStatus::NeedsLogin;
        row.message = Some("No stored login found.".to_string());
        return row;
    };
    row.plan = oauth.plan();

    if oauth.is_expired(now_ms()) {
        row.status = AccountStatus::NeedsLogin;
        row.message = Some(format!("Token expired. {relogin_hint}"));
        return row;
    }

    match usage::fetch(oauth.access_token()).await {
        Ok(windows) => row.windows = windows,
        Err(UsageError::Unauthorized) => {
            row.status = AccountStatus::NeedsLogin;
            row.message = Some(format!("Token rejected. {relogin_hint}"));
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

#[tauri::command]
async fn list_usage(app: AppHandle) -> Result<Vec<AccountUsage>, String> {
    let managed = accounts::load(&data_dir(&app)?);
    let default_email = credentials::read_default_email();

    let mut tasks = Vec::new();
    // Skip the read-only default row once the same email is a managed account.
    let default_is_managed = default_email
        .as_ref()
        .is_some_and(|email| managed.iter().any(|a| a.email.as_ref() == Some(email)));
    if !default_is_managed {
        tasks.push(tauri::async_runtime::spawn(usage_row(
            DEFAULT_ID.to_string(),
            default_email.unwrap_or_else(|| "~/.claude".to_string()),
            true,
            credentials::read_default(),
        )));
    }
    for account in managed {
        let oauth = credentials::read_managed(&account.id);
        tasks.push(tauri::async_runtime::spawn(usage_row(
            account.id.clone(),
            account.label(),
            false,
            oauth,
        )));
    }

    let mut rows = Vec::with_capacity(tasks.len());
    for task in tasks {
        rows.push(task.await.map_err(|e| e.to_string())?);
    }
    Ok(rows)
}

#[tauri::command]
async fn add_account(
    app: AppHandle,
    login_state: State<'_, LoginState>,
    email_hint: Option<String>,
) -> Result<String, String> {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    {
        let mut slot = login_state.0.lock().map_err(|e| e.to_string())?;
        if slot.is_some() {
            return Err("A login is already in progress.".to_string());
        }
        *slot = Some(cancel_tx);
    }
    let outcome = login::run(email_hint, cancel_rx).await;
    if let Ok(mut slot) = login_state.0.lock() {
        *slot = None;
    }
    let outcome = outcome?;

    let dir = data_dir(&app)?;
    let mut list = accounts::load(&dir);
    // Logging the same email in again replaces its tokens instead of duplicating the row.
    let existing = outcome
        .email
        .as_ref()
        .and_then(|email| list.iter().position(|a| a.email.as_ref() == Some(email)));
    let id = match existing {
        Some(index) => list[index].id.clone(),
        None => uuid::Uuid::new_v4().to_string(),
    };
    credentials::write_managed(&id, &outcome.oauth)?;

    let account = Account {
        id: id.clone(),
        email: outcome.email,
        org_name: outcome.org_name,
        added_at_ms: now_ms(),
    };
    let label = account.label();
    match existing {
        Some(index) => list[index] = account,
        None => list.push(account),
    }
    accounts::save(&dir, &list)?;
    Ok(label)
}

#[tauri::command]
fn cancel_login(login_state: State<'_, LoginState>) -> bool {
    login_state
        .0
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .is_some_and(|cancel| cancel.send(()).is_ok())
}

#[tauri::command]
fn remove_account(app: AppHandle, id: String) -> Result<(), String> {
    let dir = data_dir(&app)?;
    let mut list = accounts::load(&dir);
    // Only ids from our own list, so this can never target another keychain item.
    if !list.iter().any(|a| a.id == id) {
        return Err("Unknown account.".to_string());
    }
    credentials::delete_managed(&id)?;
    list.retain(|a| a.id != id);
    accounts::save(&dir, &list)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(LoginState::default())
        .invoke_handler(tauri::generate_handler![
            list_usage,
            add_account,
            cancel_login,
            remove_account
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
