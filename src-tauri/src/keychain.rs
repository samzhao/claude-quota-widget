//! macOS keychain access through the `security` CLI.
//!
//! Going through `security` rather than Security.framework means the item ACL
//! trusts `security` itself, so dev rebuilds (new binary hash each time) never
//! trigger a keychain prompt. Claude Code reads its own items the same way.

use sha2::{Digest, Sha256};
use std::{
    io::Write,
    process::{Command, Stdio},
};

const SECURITY: &str = "/usr/bin/security";
pub const CLI_DEFAULT_SERVICE: &str = "Claude Code-credentials";
pub const APP_SERVICE: &str = "Claude Quota Widget";

/// The service name Claude Code uses for a non-default config dir:
/// `Claude Code-credentials-<first 8 hex of sha256(config dir)>`.
pub fn cli_scoped_service(config_dir: &str) -> String {
    let digest = Sha256::digest(config_dir.as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{CLI_DEFAULT_SERVICE}-{hex}")
}

pub fn read(service: &str, account: &str) -> Option<String> {
    let output = Command::new(SECURITY)
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

fn is_safe_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_' | ' ' | '.'))
}

/// Writes through `security -i` on stdin so the secret never shows up in the
/// process argument list. Inputs are restricted to a quote-free alphabet
/// (callers base64 the secret) so the command line cannot be broken out of.
pub fn write(service: &str, account: &str, base64_value: &str) -> Result<(), String> {
    if !is_safe_token(service) || !is_safe_token(account) || !is_safe_token(base64_value) {
        return Err("refusing to write keychain item with unsafe characters".to_string());
    }
    let mut child = Command::new(SECURITY)
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start security: {e}"))?;
    {
        let mut stdin = child.stdin.take().ok_or("security stdin unavailable")?;
        writeln!(
            stdin,
            "add-generic-password -U -s \"{service}\" -a \"{account}\" -w \"{base64_value}\""
        )
        .map_err(|e| format!("could not write to security: {e}"))?;
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("security exited with {status}"));
    }
    // `security -i` reports per-command failures only on stderr, so confirm.
    match read(service, account) {
        Some(stored) if stored == base64_value => Ok(()),
        _ => Err("keychain write did not persist".to_string()),
    }
}

/// Returns Ok(false) when the item did not exist.
pub fn delete(service: &str, account: &str) -> Result<bool, String> {
    // Claude Code's own default login is never ours to delete.
    if service == CLI_DEFAULT_SERVICE {
        return Err("refusing to delete the default Claude Code login".to_string());
    }
    let output = Command::new(SECURITY)
        .args(["delete-generic-password", "-s", service, "-a", account])
        .output()
        .map_err(|e| e.to_string())?;
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_service_matches_claude_code_naming() {
        // First 8 hex chars of sha256 of the config dir path, as Claude Code names it.
        assert_eq!(
            cli_scoped_service("/Users/example/.claude"),
            "Claude Code-credentials-402b469b"
        );
    }

    #[test]
    fn write_rejects_quote_breakout() {
        assert!(write("svc", "acct", "abc\" -s \"other").is_err());
        assert!(write("svc\n", "acct", "abc").is_err());
    }

    #[test]
    fn delete_refuses_default_login() {
        assert!(delete(CLI_DEFAULT_SERVICE, "anyone").is_err());
    }
}
