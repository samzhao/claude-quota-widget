mod accounts;
mod cache;
mod credentials;
mod keychain;
mod login;
mod oauth;
mod settings;
mod shell;
mod tray;
mod usage;

use accounts::Account;
use cache::{Decision, Problem, UsageCache};
use credentials::Oauth;
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::oneshot;
use usage::UsageWindow;

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
    /// When `windows` was actually read from Anthropic. None = never.
    as_of_ms: Option<f64>,
    /// Earliest time an automatic check will hit the network again.
    next_check_ms: Option<f64>,
}

/// Async mutex on purpose: it is held across the fetches so two overlapping
/// `list_usage` calls cannot both spend a request on the same account.
struct CacheState(tokio::sync::Mutex<UsageCache>);

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

struct Target {
    id: String,
    label: String,
    read_only: bool,
    oauth: Option<Oauth>,
}

impl Target {
    fn relogin_hint(&self) -> &'static str {
        if self.read_only {
            "Run Claude Code on this Mac once to refresh it."
        } else {
            "Remove and re-add this account."
        }
    }

    /// A login we can actually call the endpoint with right now.
    fn usable_oauth(&self) -> Option<&Oauth> {
        self.oauth.as_ref().filter(|oauth| !oauth.is_expired(now_ms()))
    }
}

fn row_for(target: &Target, cache: &UsageCache) -> AccountUsage {
    let mut row = AccountUsage {
        id: target.id.clone(),
        label: target.label.clone(),
        plan: target.oauth.as_ref().and_then(Oauth::plan),
        read_only: target.read_only,
        status: AccountStatus::Ok,
        message: None,
        windows: Vec::new(),
        as_of_ms: None,
        next_check_ms: None,
    };
    let hint = target.relogin_hint();

    if target.oauth.is_none() {
        row.status = AccountStatus::NeedsLogin;
        row.message = Some("No stored login found.".to_string());
        return row;
    }
    if target.usable_oauth().is_none() {
        row.status = AccountStatus::NeedsLogin;
        let dead = cache.entry(&target.id).is_some_and(|entry| entry.refresh_dead);
        row.message = Some(if target.read_only || dead {
            format!("Login expired. {hint}")
        } else {
            "Token expired and could not be renewed yet. Will retry.".to_string()
        });
        return row;
    }

    let Some(entry) = cache.entry(&target.id) else {
        return row;
    };
    if entry.refresh_dead {
        row.status = AccountStatus::NeedsLogin;
        row.message = Some(format!("Login expired. {hint}"));
        return row;
    }
    row.windows = entry.windows.clone();
    row.as_of_ms = (entry.fetched_at_ms > 0.0).then_some(entry.fetched_at_ms);
    row.next_check_ms = Some(entry.next_check_ms());
    match &entry.problem {
        None => {}
        Some(Problem::Throttled) => {
            row.status = AccountStatus::RateLimited;
            row.message = Some("Anthropic is throttling usage checks.".to_string());
        }
        Some(Problem::Unauthorized) => {
            row.status = AccountStatus::NeedsLogin;
            row.message = Some(format!("Token rejected. {hint}"));
        }
        Some(Problem::Failed(message)) => {
            row.status = AccountStatus::Error;
            row.message = Some(message.clone());
        }
    }
    row
}

/// The one place usage is gathered, for both the window and the background
/// checker. The cache decides which accounts, if any, touch the network.
async fn collect_usage(app: &AppHandle, force: bool) -> Result<Vec<AccountUsage>, String> {
    let cache_state = app.state::<CacheState>();
    let managed = accounts::load(&data_dir(app)?);
    let default_email = credentials::read_default_email();

    let mut targets = Vec::new();
    // Skip the read-only default row once the same email is a managed account.
    let default_is_managed = default_email
        .as_ref()
        .is_some_and(|email| managed.iter().any(|a| a.email.as_ref() == Some(email)));
    if !default_is_managed {
        targets.push(Target {
            id: DEFAULT_ID.to_string(),
            label: default_email.unwrap_or_else(|| "~/.claude".to_string()),
            read_only: true,
            oauth: credentials::read_default(),
        });
    }
    for account in managed {
        targets.push(Target {
            oauth: credentials::read_managed(&account.id),
            label: account.label(),
            id: account.id,
            read_only: false,
        });
    }

    let mut cache = cache_state.0.lock().await;
    let ids: Vec<String> = targets.iter().map(|t| t.id.clone()).collect();
    cache.retain_ids(&ids);

    // Renew managed logins that are about to expire. Never the read-only
    // default: Claude Code owns that grant. Runs under the cache lock, so two
    // overlapping calls cannot both spend the same single-use refresh token.
    let mut renewed_any = false;
    for target in targets.iter_mut().filter(|t| !t.read_only) {
        let Some(oauth) = target.oauth.clone() else { continue };
        let now = now_ms();
        if !oauth.expires_within(now, oauth::EXPIRY_BUFFER_MS) {
            continue;
        }
        let entry = cache.entry_mut(&target.id);
        if entry.refresh_dead || now < entry.refresh_retry_at_ms {
            continue;
        }
        match oauth::refresh(&oauth, now).await {
            Ok(renewed) => {
                // Persist before use: losing a rotated refresh token logs the account out.
                if let Err(error) = credentials::write_managed(&target.id, &renewed) {
                    eprintln!("[oauth] could not store renewed login for {}: {error}", target.label);
                }
                entry.refresh_retry_at_ms = 0.0;
                target.oauth = Some(renewed);
            }
            Err(oauth::RefreshError::Dead) => entry.refresh_dead = true,
            Err(oauth::RefreshError::Transient(reason)) => {
                eprintln!("[oauth] renew failed for {}: {reason}", target.label);
                entry.refresh_retry_at_ms = now + 2.0 * 60_000.0;
            }
        }
        renewed_any = true;
    }

    // Only accounts whose reading is due touch the network; they go in parallel.
    let now = now_ms();
    let mut fetches = Vec::new();
    for target in &targets {
        let Some(oauth) = target.usable_oauth() else { continue };
        let due = cache
            .entry(&target.id)
            .map_or(Decision::Fetch, |entry| entry.decide(now, force));
        if due == Decision::Fetch {
            let token = oauth.access_token().to_string();
            let id = target.id.clone();
            fetches.push(tauri::async_runtime::spawn(async move {
                (id, usage::fetch(&token).await)
            }));
        }
    }
    let fetched_any = !fetches.is_empty();
    for fetch in fetches {
        let (id, result) = fetch.await.map_err(|e| e.to_string())?;
        cache.entry_mut(&id).record(result, now_ms());
    }
    if fetched_any || renewed_any {
        cache.save();
    }

    Ok(targets.iter().map(|target| row_for(target, &cache)).collect())
}

/// Pushes fresh rows to everything that shows them: the menubar item and,
/// if it is open, the window.
fn publish(app: &AppHandle, rows: &[AccountUsage]) {
    let candidates: Vec<tray::Candidate> = rows
        .iter()
        .filter(|row| !matches!(row.status, AccountStatus::NeedsLogin))
        .map(|row| tray::Candidate {
            label: &row.label,
            utilizations: row.windows.iter().map(|w| w.utilization).collect(),
        })
        .collect();
    shell::update_tray(app, &tray::summarize(&candidates));
    let _ = app.emit("usage-updated", rows);
}

#[tauri::command]
async fn list_usage(app: AppHandle, force: Option<bool>) -> Result<Vec<AccountUsage>, String> {
    let rows = collect_usage(&app, force.unwrap_or(false)).await?;
    publish(&app, &rows);
    Ok(rows)
}

/// Keeps readings and the menubar current while the window is hidden, where
/// page timers cannot be relied on. Sleeps until the cache says the soonest
/// account is due, so an idle app makes no requests in between.
async fn run_background_checks(app: AppHandle) {
    const SHORTEST_MS: f64 = 30_000.0;
    const LONGEST_MS: f64 = 15.0 * 60_000.0;
    loop {
        let wait_ms = match collect_usage(&app, false).await {
            Ok(rows) => {
                publish(&app, &rows);
                rows.iter()
                    .filter_map(|row| row.next_check_ms)
                    .fold(f64::INFINITY, f64::min)
                    - now_ms()
                    + 2_000.0
            }
            Err(error) => {
                eprintln!("[checks] {error}");
                60_000.0
            }
        };
        let wait_ms = if wait_ms.is_finite() { wait_ms } else { 5.0 * 60_000.0 };
        let wait = std::time::Duration::from_millis(wait_ms.clamp(SHORTEST_MS, LONGEST_MS) as u64);
        tokio::time::sleep(wait).await;
    }
}

#[tauri::command]
async fn add_account(
    app: AppHandle,
    login_state: State<'_, LoginState>,
    cache_state: State<'_, CacheState>,
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
    // Fresh tokens: drop any old reading or backoff so the next list checks right away.
    cache_state.0.lock().await.forget(&id);

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
async fn remove_account(
    app: AppHandle,
    cache_state: State<'_, CacheState>,
    id: String,
) -> Result<(), String> {
    let dir = data_dir(&app)?;
    let mut list = accounts::load(&dir);
    // Only ids from our own list, so this can never target another keychain item.
    if !list.iter().any(|a| a.id == id) {
        return Err("Unknown account.".to_string());
    }
    credentials::delete_managed(&id)?;
    list.retain(|a| a.id != id);
    accounts::save(&dir, &list)?;
    let mut cache = cache_state.0.lock().await;
    cache.forget(&id);
    cache.save();
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(LoginState::default())
        .manage(shell::UiState::default())
        .on_window_event(shell::on_window_event)
        .setup(|app| {
            let cache = match app.path().app_data_dir() {
                Ok(dir) => UsageCache::load(&dir),
                Err(_) => UsageCache::default(),
            };
            app.manage(CacheState(tokio::sync::Mutex::new(cache)));
            shell::init(app.handle())?;
            tauri::async_runtime::spawn(run_background_checks(app.handle().clone()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_usage,
            add_account,
            cancel_login,
            remove_account,
            shell::quit_app,
            shell::get_settings,
            shell::set_pinned,
            shell::set_show_dock_icon
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
