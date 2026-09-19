//! Window and app behavior the Rust side has to know before any page loads.
//! View, theme and similar display choices stay in the frontend.

use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

const FILE_NAME: &str = "settings.json";

#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Window floats above everything, on every Space.
    pub pinned: bool,
    /// Off by default: the app lives in the menubar.
    pub show_dock_icon: bool,
}

pub fn load(data_dir: &Path) -> Settings {
    fs::read_to_string(data_dir.join(FILE_NAME))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(data_dir: &Path, settings: &Settings) -> Result<(), String> {
    fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    let tmp = data_dir.join(format!("{FILE_NAME}.tmp"));
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, data_dir.join(FILE_NAME)).map_err(|e| e.to_string())
}
