//! Last known usage per account, plus the pacing rules that decide when the
//! network may be touched at all.
//!
//! The usage endpoint has a tight request budget and quota numbers move slowly,
//! so a recent reading beats polling into 429s. Persisted to disk so app
//! restarts and dev reloads do not spend fetches. Holds no token material.

use crate::usage::{UsageError, UsageWindow};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

const FILE_NAME: &str = "usage-cache.json";

const MINUTE_MS: f64 = 60_000.0;
/// Automatic checks (timer, window reopen) reuse a reading younger than this.
const AUTO_MIN_AGE_MS: f64 = 5.0 * MINUTE_MS;
/// The Refresh button may go sooner, but never into a tight loop.
const FORCED_MIN_AGE_MS: f64 = MINUTE_MS;
const THROTTLE_BACKOFF_START_MS: f64 = 5.0 * MINUTE_MS;
const THROTTLE_BACKOFF_MAX_MS: f64 = 30.0 * MINUTE_MS;
const ERROR_BACKOFF_START_MS: f64 = MINUTE_MS;
const ERROR_BACKOFF_MAX_MS: f64 = 15.0 * MINUTE_MS;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Entry {
    pub windows: Vec<UsageWindow>,
    /// When `windows` was actually read from the server. 0 = never.
    pub fetched_at_ms: f64,
    pub last_attempt_ms: f64,
    /// No fetch before this, forced or not.
    pub backoff_until_ms: f64,
    pub failures: u32,
    pub problem: Option<Problem>,
    /// Refresh token was rejected; only a new login fixes it, so stop trying.
    #[serde(default)]
    pub refresh_dead: bool,
    /// No token refresh attempt before this (after a transient failure).
    #[serde(default)]
    pub refresh_retry_at_ms: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Problem {
    Throttled,
    Unauthorized,
    Failed(String),
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    UseCache,
    Fetch,
}

fn backoff(start_ms: f64, max_ms: f64, failures: u32) -> f64 {
    let doubled = start_ms * 2f64.powi(failures.saturating_sub(1).min(16) as i32);
    doubled.min(max_ms)
}

impl Entry {
    /// After a throttle or a failed request, the backoff window is the pacing:
    /// once it closes, try again rather than also waiting out the reuse age.
    fn is_recovering(&self) -> bool {
        matches!(self.problem, Some(Problem::Throttled | Problem::Failed(_)))
    }

    pub fn decide(&self, now_ms: f64, force: bool) -> Decision {
        if now_ms < self.backoff_until_ms {
            return Decision::UseCache;
        }
        if self.is_recovering() {
            return Decision::Fetch;
        }
        let min_age = if force { FORCED_MIN_AGE_MS } else { AUTO_MIN_AGE_MS };
        if now_ms - self.last_attempt_ms < min_age {
            Decision::UseCache
        } else {
            Decision::Fetch
        }
    }

    pub fn record(&mut self, result: Result<Vec<UsageWindow>, UsageError>, now_ms: f64) {
        self.last_attempt_ms = now_ms;
        match result {
            Ok(windows) => {
                self.windows = windows;
                self.fetched_at_ms = now_ms;
                self.failures = 0;
                self.backoff_until_ms = 0.0;
                self.problem = None;
            }
            Err(UsageError::RateLimited(retry_after_secs)) => {
                self.failures += 1;
                // Never earlier than the server's Retry-After: that keeps the 429
                // alive. A first throttle trusts it as is; repeats mean it was
                // too optimistic, so our own doubling backoff becomes the floor.
                let own = |failures| {
                    backoff(THROTTLE_BACKOFF_START_MS, THROTTLE_BACKOFF_MAX_MS, failures)
                };
                let wait = match retry_after_secs.map(|secs| secs as f64 * 1000.0) {
                    None => own(self.failures),
                    Some(server) if self.failures <= 1 => server.max(MINUTE_MS),
                    Some(server) => server.max(own(self.failures - 1)),
                };
                self.backoff_until_ms = now_ms + wait;
                self.problem = Some(Problem::Throttled);
            }
            Err(UsageError::Unauthorized) => {
                self.failures += 1;
                self.backoff_until_ms = 0.0;
                self.problem = Some(Problem::Unauthorized);
            }
            Err(UsageError::Other(message)) => {
                self.failures += 1;
                self.backoff_until_ms =
                    now_ms + backoff(ERROR_BACKOFF_START_MS, ERROR_BACKOFF_MAX_MS, self.failures);
                self.problem = Some(Problem::Failed(message));
            }
        }
    }

    /// Earliest moment a non-forced check would hit the network again.
    pub fn next_check_ms(&self) -> f64 {
        if self.is_recovering() {
            self.backoff_until_ms
        } else {
            self.last_attempt_ms + AUTO_MIN_AGE_MS
        }
    }
}

#[derive(Default)]
pub struct UsageCache {
    path: Option<PathBuf>,
    entries: HashMap<String, Entry>,
}

impl UsageCache {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(FILE_NAME);
        let entries = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries,
        }
    }

    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.get(id)
    }

    pub fn entry_mut(&mut self, id: &str) -> &mut Entry {
        self.entries.entry(id.to_string()).or_default()
    }

    pub fn forget(&mut self, id: &str) {
        self.entries.remove(id);
    }

    pub fn retain_ids(&mut self, keep: &[String]) {
        self.entries.retain(|id, _| keep.contains(id));
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        // Write-then-rename so a crash mid-write cannot corrupt the cache.
        let tmp = path.with_extension("json.tmp");
        if let Ok(json) = serde_json::to_string(&self.entries) {
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(&tmp, path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: f64 = 1_000_000_000.0;

    fn window(pct: f64) -> UsageWindow {
        UsageWindow {
            key: "session".into(),
            label: "5-hour".into(),
            utilization: pct,
            resets_at: None,
            severity: None,
        }
    }

    #[test]
    fn fresh_reading_is_reused_and_force_only_shortens_the_wait() {
        let mut entry = Entry::default();
        assert_eq!(entry.decide(T0, false), Decision::Fetch);

        entry.record(Ok(vec![window(4.0)]), T0);
        assert_eq!(entry.decide(T0 + 4.0 * MINUTE_MS, false), Decision::UseCache);
        assert_eq!(entry.decide(T0 + 5.0 * MINUTE_MS, false), Decision::Fetch);
        assert_eq!(entry.decide(T0 + 0.5 * MINUTE_MS, true), Decision::UseCache);
        assert_eq!(entry.decide(T0 + 1.0 * MINUTE_MS, true), Decision::Fetch);
    }

    #[test]
    fn throttle_keeps_last_reading_and_blocks_even_forced_checks() {
        let mut entry = Entry::default();
        entry.record(Ok(vec![window(78.0)]), T0);

        let t1 = T0 + 10.0 * MINUTE_MS;
        entry.record(Err(UsageError::RateLimited(None)), t1);
        assert_eq!(entry.windows, vec![window(78.0)]);
        assert_eq!(entry.fetched_at_ms, T0);
        assert_eq!(entry.problem, Some(Problem::Throttled));
        assert_eq!(entry.decide(t1 + 4.9 * MINUTE_MS, true), Decision::UseCache);
        assert_eq!(entry.decide(t1 + 5.0 * MINUTE_MS, true), Decision::Fetch);

        // Second 429 in a row doubles the wait; a later success clears it.
        let t2 = t1 + 5.0 * MINUTE_MS;
        entry.record(Err(UsageError::RateLimited(None)), t2);
        assert_eq!(entry.backoff_until_ms, t2 + 10.0 * MINUTE_MS);
        entry.record(Ok(vec![window(80.0)]), t2 + 10.0 * MINUTE_MS);
        assert_eq!(entry.failures, 0);
        assert_eq!(entry.problem, None);
    }

    #[test]
    fn retry_after_header_wins_over_our_own_backoff() {
        let mut entry = Entry::default();
        entry.record(Err(UsageError::RateLimited(Some(1200))), T0);
        assert_eq!(entry.backoff_until_ms, T0 + 20.0 * MINUTE_MS);
        assert_eq!(entry.next_check_ms(), T0 + 20.0 * MINUTE_MS);
    }

    #[test]
    fn throttled_account_retries_when_the_servers_window_closes() {
        let mut entry = Entry::default();
        entry.record(Err(UsageError::RateLimited(Some(84))), T0);
        let reopened = T0 + 84_000.0;
        assert_eq!(entry.decide(reopened - 1.0, true), Decision::UseCache);
        // Not forced, and well inside the 5-minute reuse age: still due.
        assert_eq!(entry.decide(reopened, false), Decision::Fetch);
        assert_eq!(entry.next_check_ms(), reopened);

        // Throttled again straight away: the short Retry-After is no longer trusted.
        entry.record(Err(UsageError::RateLimited(Some(84))), reopened);
        assert_eq!(entry.backoff_until_ms, reopened + 5.0 * MINUTE_MS);
    }

    #[test]
    fn rejected_token_does_not_retry_in_a_loop() {
        let mut entry = Entry::default();
        entry.record(Err(UsageError::Unauthorized), T0);
        assert_eq!(entry.decide(T0 + 1.0, false), Decision::UseCache);
        assert_eq!(entry.decide(T0 + 5.0 * MINUTE_MS, false), Decision::Fetch);
    }

    #[test]
    fn backoff_is_capped() {
        let mut entry = Entry::default();
        for i in 0..12 {
            entry.record(Err(UsageError::RateLimited(None)), T0 + i as f64);
        }
        assert_eq!(entry.backoff_until_ms, T0 + 11.0 + THROTTLE_BACKOFF_MAX_MS);
    }
}
