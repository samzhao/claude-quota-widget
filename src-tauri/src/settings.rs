//! Window and app behavior the Rust side has to know before any page loads.
//! View, theme and similar display choices stay in the frontend.

use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

const FILE_NAME: &str = "settings.json";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Window floats above everything, on every Space.
    pub pinned: bool,
    /// Off by default: the app lives in the menubar.
    pub show_dock_icon: bool,
    /// Logical pixels. Always remembered.
    pub width: f64,
    /// Set once the user resizes the window by hand. None = height follows content.
    pub manual_height: Option<f64>,
}

pub const DEFAULT_WIDTH: f64 = 470.0;

impl Default for Settings {
    fn default() -> Self {
        Self {
            pinned: false,
            show_dock_icon: false,
            width: DEFAULT_WIDTH,
            manual_height: None,
        }
    }
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
