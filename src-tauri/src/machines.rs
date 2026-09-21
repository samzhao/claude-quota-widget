//! Optional: other Macs (or Linux boxes) you reach over SSH. The app asks each
//! one which Claude account it is signed in to, so the matching account can
//! show "this one is in use on the studio desktop".
//!
//! Opt-in: nothing here runs until a machine is added. It uses your existing
//! SSH setup (keys, agent, `~/.ssh/config`) in batch mode, so it can never
//! prompt, and it only ever reads: `claude auth status` and the process list.
//! Only an email address and a process count come back. No tokens.

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

const FILE_NAME: &str = "machines.json";
const MAX_MACHINES: usize = 8;
const MAX_NAME_CHARS: usize = 24;
const MAX_TARGET_CHARS: usize = 128;
const CONNECT_TIMEOUT_SECS: u32 = 6;
/// Each profile costs one `claude` start-up on the far side, so allow for a few.
const PROBE_TIMEOUT: Duration = Duration::from_secs(45);
/// Caps on what a machine may send back. A machine you trusted once could be
/// compromised later, so its reply is treated as untrusted input: bounded in
/// total size, in number of sightings, and in the length of each string.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SIGHTINGS: usize = 16;
const MAX_FIELD_CHARS: usize = 128;

/// Runs on the remote under plain `sh` (a zsh login shell aborts on unmatched
/// globs). Looks at the default config dir plus the two multi-profile layouts
/// in common use, skipping folders Claude Code has never written a config to.
const PROBE_SCRIPT: &str = r#"
PATH="$HOME/.local/bin:$HOME/.claude/local:/opt/homebrew/bin:/usr/local/bin:$PATH"
command -v claude >/dev/null 2>&1 || { echo "CQW_NO_CLAUDE"; exit 0; }
for d in "$HOME/.claude" "$HOME"/claude-* "$HOME"/.claude-profiles/*; do
  [ -d "$d" ] || continue
  if [ "$d" = "$HOME/.claude" ]; then
    out=$(claude auth status --json 2>/dev/null)
  else
    [ -f "$d/.claude.json" ] || [ -f "$d/.credentials.json" ] || continue
    out=$(CLAUDE_CONFIG_DIR="$d" claude auth status --json 2>/dev/null)
  fi
  printf 'CQW_PROFILE\t%s\t%s\n' "$d" "$(printf '%s' "$out" | tr -d '\n\t')"
done
for pid in $(pgrep -x claude 2>/dev/null); do
  dir=$(ps eww -o command= -p "$pid" 2>/dev/null | tr ' ' '\n' | sed -n 's/^CLAUDE_CONFIG_DIR=//p' | head -1)
  printf 'CQW_PROC\t%s\n' "${dir:-$HOME/.claude}"
done
printf 'CQW_HOME\t%s\n' "$HOME"
echo "CQW_DONE"
"#;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Machine {
    pub name: String,
    /// Anything `ssh` accepts as a destination: `user@host` or a config alias.
    pub ssh: String,
}

/// One signed-in profile found on a machine.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Sighting {
    pub email: String,
    /// Config dir with the home folder shortened to `~`.
    pub profile: String,
    /// `claude` processes currently running under this profile.
    pub running: u32,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Report {
    pub sightings: Vec<Sighting>,
    pub problem: Option<String>,
    pub checked_at_ms: f64,
}

pub fn load(data_dir: &Path) -> Vec<Machine> {
    fs::read_to_string(data_dir.join(FILE_NAME))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(data_dir: &Path, machines: &[Machine]) -> Result<(), String> {
    fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(machines).map_err(|e| e.to_string())?;
    let tmp = data_dir.join(format!("{FILE_NAME}.tmp"));
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, data_dir.join(FILE_NAME)).map_err(|e| e.to_string())
}

/// The destination is handed to `ssh` as a single argument, never through a
/// shell. This keeps it from being read as an option (`-oProxyCommand=…`) and
/// limits it to characters a host, user or config alias can contain.
pub fn validate(name: &str, ssh: &str, existing: &[Machine]) -> Result<Machine, String> {
    let name = crate::labels::clean(name)
        .map(|n| n.chars().take(MAX_NAME_CHARS).collect::<String>())
        .ok_or("Give the machine a name.")?;
    let ssh = ssh.trim();
    if ssh.is_empty() {
        return Err("Enter an SSH destination, like user@host.".to_string());
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | ':' | '[' | ']' | '%');
    if ssh.starts_with('-') || ssh.chars().count() > MAX_TARGET_CHARS || !ssh.chars().all(allowed) {
        return Err("That SSH destination has characters that are not allowed.".to_string());
    }
    if existing.iter().any(|m| m.name.eq_ignore_ascii_case(&name)) {
        return Err(format!("A machine named {name} already exists."));
    }
    if existing.len() >= MAX_MACHINES {
        return Err(format!("At most {MAX_MACHINES} machines."));
    }
    Ok(Machine { name, ssh: ssh.to_string() })
}

/// Turns the probe's output into sightings. Pure, so it can be tested without SSH.
fn cap(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).take(MAX_FIELD_CHARS).collect()
}

pub fn parse_probe(output: &str) -> Result<Vec<Sighting>, String> {
    if output.lines().any(|l| l.trim() == "CQW_NO_CLAUDE") {
        return Err("Claude Code is not installed there (no `claude` command).".to_string());
    }
    if !output.lines().any(|l| l.trim() == "CQW_DONE") {
        return Err("The check did not finish.".to_string());
    }
    let home = output
        .lines()
        .find_map(|l| l.strip_prefix("CQW_HOME\t"))
        .unwrap_or("")
        .trim()
        .to_string();
    let tidy = |dir: &str| match dir.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => dir.to_string(),
    };

    let mut running: HashMap<String, u32> = HashMap::new();
    for dir in output.lines().filter_map(|l| l.strip_prefix("CQW_PROC\t")) {
        *running.entry(dir.trim().trim_end_matches('/').to_string()).or_default() += 1;
    }

    let mut sightings = Vec::new();
    for line in output.lines() {
        let Some(rest) = line.strip_prefix("CQW_PROFILE\t") else { continue };
        let Some((dir, json)) = rest.split_once('\t') else { continue };
        let Ok(status) = serde_json::from_str::<serde_json::Value>(json) else { continue };
        if status.get("loggedIn").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let Some(email) = status.get("email").and_then(|v| v.as_str()) else { continue };
        let dir = dir.trim().trim_end_matches('/');
        sightings.push(Sighting {
            email: cap(email.trim()).to_lowercase(),
            profile: cap(&tidy(dir)),
            running: running.get(dir).copied().unwrap_or(0),
        });
        if sightings.len() >= MAX_SIGHTINGS {
            break;
        }
    }
    Ok(sightings)
}

fn explain_ssh_failure(stderr: &str, target: &str) -> String {
    let lower = stderr.to_lowercase();
    if lower.contains("host key verification failed") {
        format!("This Mac does not trust that host yet. Run `ssh {target}` once in Terminal and accept its key.")
    } else if lower.contains("permission denied") {
        "SSH key login is not set up for that host (password logins are never attempted).".to_string()
    } else if lower.contains("timed out") || lower.contains("no route") || lower.contains("unreachable") || lower.contains("connection refused") {
        "Unreachable. It may be asleep, off, or on another network.".to_string()
    } else if lower.contains("could not resolve") {
        "That host name does not resolve.".to_string()
    } else {
        let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("ssh failed");
        first.chars().take(140).collect()
    }
}

pub async fn probe(machine: &Machine) -> Result<Vec<Sighting>, String> {
    let mut child = Command::new("/usr/bin/ssh")
        .args([
            "-o", "BatchMode=yes", // never prompt for passwords or host keys
            "-o", &format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
            "-o", "ServerAliveInterval=5",
            "-o", "ServerAliveCountMax=3",
            "-T",
            "--",
            &machine.ssh,
            "sh -s",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start ssh: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(PROBE_SCRIPT.as_bytes()).await;
        // Dropping stdin sends EOF, which is what ends `sh -s`.
    }
    let output = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "The check timed out.".to_string())?
        .map_err(|e| e.to_string())?;

    let mut bytes = output.stdout;
    bytes.truncate(MAX_OUTPUT_BYTES);
    let stdout = String::from_utf8_lossy(&bytes);
    if !output.status.success() && !stdout.contains("CQW_DONE") {
        return Err(explain_ssh_failure(&String::from_utf8_lossy(&output.stderr), &machine.ssh));
    }
    parse_probe(&stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "CQW_PROFILE\t/Users/pat/.claude\t{\"loggedIn\":true,\"email\":\"Work@Example.com\",\"subscriptionType\":\"max\"}\n\
CQW_PROFILE\t/Users/pat/.claude-profiles/1\t{\"loggedIn\":true,\"email\":\"side@example.com\"}\n\
CQW_PROFILE\t/Users/pat/.claude-profiles/2\t{\"loggedIn\":false}\n\
CQW_PROC\t/Users/pat/.claude\n\
CQW_PROC\t/Users/pat/.claude\n\
CQW_PROC\t/Users/pat/.claude-profiles/1/\n\
CQW_HOME\t/Users/pat\n\
CQW_DONE\n";

    #[test]
    fn parses_profiles_and_counts_running_sessions_per_profile() {
        let sightings = parse_probe(SAMPLE).unwrap();
        assert_eq!(
            sightings,
            vec![
                Sighting { email: "work@example.com".into(), profile: "~/.claude".into(), running: 2 },
                Sighting { email: "side@example.com".into(), profile: "~/.claude-profiles/1".into(), running: 1 },
            ]
        );
    }

    #[test]
    fn a_cut_off_or_clauseless_check_is_an_error_not_an_empty_result() {
        assert!(parse_probe("CQW_PROFILE\t/x\t{}\n").is_err());
        assert!(parse_probe("CQW_NO_CLAUDE\n").unwrap_err().contains("not installed"));
        assert_eq!(parse_probe("CQW_HOME\t/h\nCQW_DONE\n").unwrap(), vec![]);
    }

    #[test]
    fn a_hostile_remote_cannot_flood_us() {
        let mut probe = String::new();
        for i in 0..40 {
            probe.push_str(&format!(
                "CQW_PROFILE\t/Users/pat/p{i}\t{{\"loggedIn\":true,\"email\":\"{}@example.com\"}}\n",
                "x".repeat(500)
            ));
        }
        probe.push_str("CQW_HOME\t/Users/pat\nCQW_DONE\n");
        let sightings = parse_probe(&probe).unwrap();
        assert_eq!(sightings.len(), MAX_SIGHTINGS);
        assert!(sightings.iter().all(|s| s.email.chars().count() <= MAX_FIELD_CHARS));
    }

    #[test]
    fn control_characters_from_a_remote_are_stripped() {
        let probe = "CQW_PROFILE\t/Users/pat/.claude\t{\"loggedIn\":true,\"email\":\"a\\u0007b@example.com\"}\nCQW_HOME\t/Users/pat\nCQW_DONE\n";
        assert_eq!(parse_probe(probe).unwrap()[0].email, "ab@example.com");
    }

    #[test]
    fn ssh_destination_cannot_smuggle_options_or_shell() {
        let none: Vec<Machine> = vec![];
        assert!(validate("mini", "-oProxyCommand=evil", &none).is_err());
        assert!(validate("mini", "host; rm -rf ~", &none).is_err());
        assert!(validate("mini", "host $(id)", &none).is_err());
        assert!(validate("", "user@host", &none).is_err());
        assert_eq!(
            validate(" studio ", " pat@10.0.0.5 ", &none).unwrap(),
            Machine { name: "studio".into(), ssh: "pat@10.0.0.5".into() }
        );
        assert!(validate("alias", "my-mac.local", &none).is_ok());
        let one = vec![Machine { name: "Studio".into(), ssh: "a@b".into() }];
        assert!(validate("studio", "c@d", &one).is_err());
    }

    #[test]
    fn failures_are_explained_in_plain_words() {
        assert!(explain_ssh_failure("Host key verification failed.", "a@b").contains("ssh a@b"));
        assert!(explain_ssh_failure("a@b: Permission denied (publickey).", "a@b").contains("key login"));
        assert!(explain_ssh_failure("ssh: connect to host x port 22: Operation timed out", "x").contains("Unreachable"));
    }
}
