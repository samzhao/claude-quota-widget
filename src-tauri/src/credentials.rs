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

    pub fn refresh_token(&self) -> Option<&str> {
        self.0
            .get("refreshToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
    }

    /// True when the access token is expired or will be within `buffer_ms`.
    /// No usable expiry counts as expiring, so it gets refreshed rather than
    /// trusted forever.
    pub fn expires_within(&self, now_ms: f64, buffer_ms: f64) -> bool {
        self.expires_at().is_none_or(|at| now_ms + buffer_ms >= at)
    }

    /// Merges a token endpoint response into a copy of these credentials.
    /// Refresh tokens can be single-use, so a rotated one must replace the old
    /// one; when the server sends none, the existing one stays valid.
    pub fn with_refresh_response(&self, response: &Value, now_ms: f64) -> Option<Self> {
        let access_token = response
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())?;
        let mut next = self.0.clone();
        let fields = next.as_object_mut()?;
        fields.insert("accessToken".into(), Value::from(access_token));
        if let Some(seconds) = response.get("expires_in").and_then(Value::as_f64) {
            fields.insert("expiresAt".into(), Value::from(now_ms + seconds * 1000.0));
        }
        if let Some(rotated) = response
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
        {
            fields.insert("refreshToken".into(), Value::from(rotated));
        }
        if let Some(scope) = response.get("scope").and_then(Value::as_str) {
            let scopes: Vec<Value> = scope.split_whitespace().map(Value::from).collect();
            if !scopes.is_empty() {
                fields.insert("scopes".into(), Value::from(scopes));
            }
        }
        Some(Self(next))
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

    fn sample() -> Oauth {
        Oauth::from_credentials_json(
            r#"{"claudeAiOauth":{"accessToken":"old-access","refreshToken":"old-refresh","expiresAt":1000000,"subscriptionType":"max","scopes":["a"]}}"#,
        )
        .expect("parse")
    }

    #[test]
    fn refresh_response_rotates_tokens_and_keeps_everything_else() {
        let response = serde_json::json!({
            "access_token": "new-access", "refresh_token": "new-refresh",
            "expires_in": 28800, "scope": "a b"
        });
        let next = sample().with_refresh_response(&response, 2_000_000.0).expect("merge");
        assert_eq!(next.access_token(), "new-access");
        assert_eq!(next.refresh_token(), Some("new-refresh"));
        assert_eq!(next.expires_at(), Some(2_000_000.0 + 28_800_000.0));
        assert_eq!(next.plan().as_deref(), Some("max"));
        assert_eq!(next.0["scopes"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn refresh_response_without_rotation_keeps_the_old_refresh_token() {
        let response = serde_json::json!({ "access_token": "new-access", "expires_in": 60 });
        let next = sample().with_refresh_response(&response, 0.0).expect("merge");
        assert_eq!(next.refresh_token(), Some("old-refresh"));
        assert!(sample().with_refresh_response(&serde_json::json!({}), 0.0).is_none());
    }

    #[test]
    fn expiring_soon_counts_as_expiring() {
        let oauth = sample(); // expires at 1_000_000
        assert!(!oauth.expires_within(0.0, 300_000.0));
        assert!(oauth.expires_within(700_000.0, 300_000.0));
        assert!(oauth.expires_within(1_500_000.0, 300_000.0));
    }

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
