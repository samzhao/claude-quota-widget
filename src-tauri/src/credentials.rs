//! Reads and stores Claude OAuth credentials. Token values never leave the Rust side.

use crate::keychain;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use std::{fs, path::PathBuf};

/// The `claudeAiOauth` object, kept as raw JSON so fields this app does not
/// know about survive a store/refresh round trip.
#[derive(Clone)]
pub struct Oauth(Value);

impl Oauth {
    /// Accepts a full credentials document (`{ "claudeAiOauth": { … } }`).
    pub fn from_credentials_json(json: &str) -> Option<Self> {
        let mut doc: Value = serde_json::from_str(json).ok()?;
        let oauth = doc.get_mut("claudeAiOauth")?.take();
        let candidate = Self(oauth);
        (!candidate.access_token().trim().is_empty()).then_some(candidate)
    }

    pub fn access_token(&self) -> &str {
        self.0.get("accessToken").and_then(Value::as_str).unwrap_or("")
    }

    /// Epoch milliseconds.
    pub fn expires_at(&self) -> Option<f64> {
        self.0.get("expiresAt").and_then(Value::as_f64)
    }

    pub fn plan(&self) -> Option<String> {
        self.0
            .get("subscriptionType")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub fn is_expired(&self, now_ms: f64) -> bool {
        self.expires_at().is_some_and(|at| at <= now_ms)
    }

    fn to_credentials_json(&self) -> String {
        serde_json::json!({ "claudeAiOauth": self.0 }).to_string()
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn mac_user() -> Option<String> {
    std::env::var("USER").ok()
}

/// The default `~/.claude` login. Read-only: Claude Code on this Mac owns this
/// grant, so refreshing it here would rotate the token out from under it.
pub fn read_default() -> Option<Oauth> {
    let from_keychain = mac_user()
        .and_then(|user| keychain::read(keychain::CLI_DEFAULT_SERVICE, &user))
        .and_then(|json| Oauth::from_credentials_json(&json));
    let from_file = home()
        .and_then(|h| fs::read_to_string(h.join(".claude/.credentials.json")).ok())
        .and_then(|json| Oauth::from_credentials_json(&json));

    // Both can exist; whichever expires later is the one Claude Code refreshed last.
    match (from_keychain, from_file) {
        (Some(k), Some(f)) => {
            if f.expires_at().unwrap_or(0.0) > k.expires_at().unwrap_or(0.0) {
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
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("oauthAccount")?
        .get("emailAddress")?
        .as_str()
        .map(str::to_string)
}

/// What a just-finished `claude auth login` left behind for `config_dir`.
pub fn read_cli_scoped(config_dir: &str) -> Option<Oauth> {
    let from_keychain = mac_user()
        .and_then(|user| keychain::read(&keychain::cli_scoped_service(config_dir), &user))
        .and_then(|json| Oauth::from_credentials_json(&json));
    from_keychain.or_else(|| {
        fs::read_to_string(PathBuf::from(config_dir).join(".credentials.json"))
            .ok()
            .and_then(|json| Oauth::from_credentials_json(&json))
    })
}

pub fn delete_cli_scoped(config_dir: &str) {
    if let Some(user) = mac_user() {
        let _ = keychain::delete(&keychain::cli_scoped_service(config_dir), &user);
    }
}

pub fn read_managed(account_id: &str) -> Option<Oauth> {
    let encoded = keychain::read(keychain::APP_SERVICE, account_id)?;
    let json = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    Oauth::from_credentials_json(&json)
}

pub fn write_managed(account_id: &str, oauth: &Oauth) -> Result<(), String> {
    let encoded = STANDARD.encode(oauth.to_credentials_json());
    keychain::write(keychain::APP_SERVICE, account_id, &encoded)
}

pub fn delete_managed(account_id: &str) -> Result<(), String> {
    keychain::delete(keychain::APP_SERVICE, account_id).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real login keychain with a dummy item, so opt-in only.
    /// Run with: cargo test -- --ignored
    #[test]
    #[ignore]
    fn managed_credentials_round_trip_through_keychain() {
        let id = format!("selftest-{}", uuid::Uuid::new_v4());
        let json = r#"{"claudeAiOauth":{"accessToken":"dummy \"quoted\" token","refreshToken":"r","expiresAt":1789806118204,"subscriptionType":"max","futureField":[1,2]}}"#;
        let oauth = Oauth::from_credentials_json(json).expect("parse");

        write_managed(&id, &oauth).expect("write");
        let stored = read_managed(&id).expect("read back");
        assert_eq!(stored.access_token(), "dummy \"quoted\" token");
        assert_eq!(stored.plan().as_deref(), Some("max"));
        assert_eq!(stored.0.get("futureField"), oauth.0.get("futureField"));

        delete_managed(&id).expect("delete");
        assert!(read_managed(&id).is_none());
    }
}
