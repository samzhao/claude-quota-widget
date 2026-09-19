//! Free-text labels people attach to accounts ("work laptop", "main", …).
//! Kept apart from the account list because the read-only default login can
//! carry one too, and it is not in that list.

use std::{collections::HashMap, fs, path::Path};

const FILE_NAME: &str = "labels.json";
pub const MAX_CHARS: usize = 32;

pub type Labels = HashMap<String, String>;

pub fn load(data_dir: &Path) -> Labels {
    fs::read_to_string(data_dir.join(FILE_NAME))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(data_dir: &Path, labels: &Labels) -> Result<(), String> {
    fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(labels).map_err(|e| e.to_string())?;
    let tmp = data_dir.join(format!("{FILE_NAME}.tmp"));
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, data_dir.join(FILE_NAME)).map_err(|e| e.to_string())
}

/// Whatever was typed, made safe to show in a small chip: one line, no
/// control characters, single spaces, at most `MAX_CHARS`. None = no label.
pub fn clean(input: &str) -> Option<String> {
    let collapsed = input
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let capped: String = collapsed.chars().take(MAX_CHARS).collect();
    let capped = capped.trim_end().to_string();
    (!capped.is_empty()).then_some(capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_keeps_ordinary_text_and_emoji() {
        assert_eq!(clean("  mac mini  "), Some("mac mini".to_string()));
        assert_eq!(clean("travail 🧳"), Some("travail 🧳".to_string()));
    }

    #[test]
    fn clean_flattens_newlines_and_runs_of_spaces() {
        assert_eq!(clean("a\n\tb   c"), Some("a b c".to_string()));
    }

    #[test]
    fn clean_caps_length_by_characters_not_bytes() {
        let long = "é".repeat(50);
        assert_eq!(clean(&long).unwrap().chars().count(), MAX_CHARS);
    }

    #[test]
    fn blank_means_no_label() {
        assert_eq!(clean("   \n "), None);
        assert_eq!(clean(""), None);
    }
}
