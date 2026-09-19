//! The list of accounts this app manages. Labels only; secrets live in the keychain.

use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

const FILE_NAME: &str = "accounts.json";

#[derive(Serialize, Deserialize, Clone)]
pub struct Account {
    pub id: String,
    pub email: Option<String>,
    pub org_name: Option<String>,
    pub added_at_ms: f64,
}

impl Account {
    pub fn label(&self) -> String {
        self.email
            .clone()
            .unwrap_or_else(|| format!("account {}", &self.id[..8.min(self.id.len())]))
    }
}

pub fn load(data_dir: &Path) -> Vec<Account> {
    fs::read_to_string(data_dir.join(FILE_NAME))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(data_dir: &Path, accounts: &[Account]) -> Result<(), String> {
    fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(accounts).map_err(|e| e.to_string())?;
    // Write-then-rename so a crash mid-write cannot truncate the list.
    let tmp = data_dir.join(format!("{FILE_NAME}.tmp"));
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, data_dir.join(FILE_NAME)).map_err(|e| e.to_string())
}
