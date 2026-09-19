//! Reads Claude Code OAuth credentials. Token values never leave the Rust side.

use serde::Deserialize;
use std::{fs, path::PathBuf, process::Command};

const DEFAULT_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

#[derive(Deserialize)]
struct CredentialsJson {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthBlob>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OauthBlob {
    pub access_token: String,
    /// Epoch milliseconds.
    pub expires_at: Option<f64>,
    pub subscription_type: Option<String>,
}

impl OauthBlob {
    pub fn is_expired(&self, now_ms: f64) -> bool {
        self.expires_at.is_some_and(|at| at <= now_ms)
    }
}

fn parse(json: &str) -> Option<OauthBlob> {
    serde_json::from_str::<CredentialsJson>(json)
        .ok()?
        .claude_ai_oauth
        .filter(|o| !o.access_token.trim().is_empty())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Goes through the `security` CLI rather than Security.framework because the
/// item's ACL already trusts `security` (Claude Code reads it the same way),
/// so no keychain prompt appears and dev rebuilds don't re-trigger one.
fn read_keychain(service: &str, account: &str) -> Option<String> {
    let output = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let secret = String::from_utf8(output.stdout).ok()?;
    Some(secret.trim().to_string())
}

/// The default `~/.claude` login. Read-only: Claude Code on this Mac owns this
/// grant, so refreshing it here would rotate the token out from under it.
pub fn read_default() -> Option<OauthBlob> {
    let from_keychain = std::env::var("USER")
        .ok()
        .and_then(|user| read_keychain(DEFAULT_KEYCHAIN_SERVICE, &user))
        .and_then(|json| parse(&json));
    let from_file = home()
        .and_then(|h| fs::read_to_string(h.join(".claude/.credentials.json")).ok())
        .and_then(|json| parse(&json));

    // Both can exist; whichever expires later is the one Claude Code refreshed last.
    match (from_keychain, from_file) {
        (Some(k), Some(f)) => {
            if f.expires_at.unwrap_or(0.0) > k.expires_at.unwrap_or(0.0) {
                Some(f)
            } else {
                Some(k)
            }
        }
        (k, f) => k.or(f),
    }
}

/// Email of the default login, from `~/.claude.json`. Used only as a row label.
pub fn read_default_email() -> Option<String> {
    let raw = fs::read_to_string(home()?.join(".claude.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("oauthAccount")?
        .get("emailAddress")?
        .as_str()
        .map(str::to_string)
}
