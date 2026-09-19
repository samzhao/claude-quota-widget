//! Adds an account by driving the official `claude` CLI login in a throwaway
//! config dir, then lifting the resulting tokens into this app's own keychain
//! service. The default `~/.claude` login is never read, written or deleted here.

use crate::credentials::{self, Oauth};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, sync::oneshot};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
const STATUS_TIMEOUT: Duration = Duration::from_secs(20);

pub struct LoginOutcome {
    pub email: Option<String>,
    pub org_name: Option<String>,
    pub oauth: Oauth,
}

/// A bundled .app does not inherit the shell PATH, so look in the usual spots
/// before asking a login shell.
fn resolve_claude() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let candidates = [
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ];
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Ok(found);
    }
    let output = std::process::Command::new("/bin/zsh")
        .args(["-lc", "command -v claude"])
        .output()
        .map_err(|e| e.to_string())?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && Path::new(&path).is_file() {
        return Ok(PathBuf::from(path));
    }
    Err("Could not find the `claude` CLI. Install Claude Code first.".to_string())
}

fn claude_command(claude: &Path, config_dir: &str) -> Command {
    let mut command = Command::new(claude);
    command
        .env("CLAUDE_CONFIG_DIR", config_dir)
        // Claude Code 2.1.220+ hashes this one for the keychain service name.
        .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", config_dir)
        .kill_on_drop(true);
    command
}

fn create_temp_config_dir() -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!("cqw-login-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).map_err(|e| format!("could not create temp dir: {e}"))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    // /var -> /private/var: the CLI hashes the path it is given, so hand it
    // the same canonical string we hash when looking the item up afterwards.
    let canonical = fs::canonicalize(&dir).unwrap_or(dir);
    canonical
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "temp dir path is not valid UTF-8".to_string())
}

fn validate_email_hint(hint: Option<String>) -> Result<Option<String>, String> {
    let Some(email) = hint.map(|h| h.trim().to_string()).filter(|h| !h.is_empty()) else {
        return Ok(None);
    };
    let plausible = email.contains('@')
        && !email.starts_with('-')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control());
    plausible
        .then_some(Some(email))
        .ok_or_else(|| "That does not look like an email address.".to_string())
}

fn read_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn read_identity(claude: &Path, config_dir: &str) -> (Option<String>, Option<String>) {
    let mut command = claude_command(claude, config_dir);
    command.args(["auth", "status", "--json"]).stdin(Stdio::null());
    let status: Value = match tokio::time::timeout(STATUS_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => serde_json::from_slice(&output.stdout).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    (
        read_string(&status, &["email", "emailAddress"]),
        read_string(&status, &["orgName", "organizationName"]),
    )
}

async fn login_in_dir(
    claude: &Path,
    config_dir: &str,
    email_hint: Option<String>,
    cancel: oneshot::Receiver<()>,
) -> Result<LoginOutcome, String> {
    let mut command = claude_command(claude, config_dir);
    command.args(["auth", "login", "--claudeai"]);
    if let Some(email) = &email_hint {
        command.args(["--email", email]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start claude: {e}"))?;
    // The CLI ties its OAuth callback listener to stdin staying open, so hold
    // the handle until the process exits.
    let _stdin = child.stdin.take();

    let status = tokio::select! {
        result = tokio::time::timeout(LOGIN_TIMEOUT, child.wait()) => match result {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => return Err(format!("claude login failed: {e}")),
            Err(_) => return Err("Login timed out after 3 minutes.".to_string()),
        },
        _ = cancel => return Err("Login cancelled.".to_string()),
    };
    if !status.success() {
        return Err("Login did not complete. Nothing was saved.".to_string());
    }

    let oauth = credentials::read_cli_scoped(config_dir)
        .ok_or("Login finished but no credentials were found. Is Claude Code up to date?")?;
    let (email, org_name) = read_identity(claude, config_dir).await;
    Ok(LoginOutcome {
        email: email.or(email_hint),
        org_name,
        oauth,
    })
}

pub async fn run(
    email_hint: Option<String>,
    cancel: oneshot::Receiver<()>,
) -> Result<LoginOutcome, String> {
    let email_hint = validate_email_hint(email_hint)?;
    let claude = resolve_claude()?;
    let config_dir = create_temp_config_dir()?;

    let outcome = login_in_dir(&claude, &config_dir, email_hint, cancel).await;

    // Always clean up, success or not: the scoped keychain item and the temp dir.
    credentials::delete_cli_scoped(&config_dir);
    let _ = fs::remove_dir_all(&config_dir);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_hint_cannot_smuggle_flags() {
        assert!(validate_email_hint(Some("--console".into())).is_err());
        assert!(validate_email_hint(Some("-x@y.com".into())).is_err());
        assert!(validate_email_hint(Some("a b@y.com".into())).is_err());
        assert_eq!(validate_email_hint(Some("  ".into())), Ok(None));
        assert_eq!(
            validate_email_hint(Some(" sam@example.com ".into())),
            Ok(Some("sam@example.com".to_string()))
        );
    }
}
