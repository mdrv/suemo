//! Parsed `~/.config/suemo/config.toml`.
//!
//! Missing file = all defaults; invalid file = hard error (the daemon
//! refuses to boot with a broken config); unknown keys are ignored.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: Ui,
}

impl Default for Config {
    fn default() -> Self {
        Self { ui: Ui::default() }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Ui {
    /// Grid snap for create/move/resize, in minutes (decisions.md Q1).
    pub snap_minutes: u32,
}

impl Default for Ui {
    fn default() -> Self {
        Self { snap_minutes: 5 }
    }
}

pub fn default_path() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"),
    };
    base.join("suemo").join("config.toml")
}

pub fn load() -> Result<Config> {
    load_from(&default_path())
}

pub fn load_from(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    };
    let config: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    ensure!(
        matches!(config.ui.snap_minutes, 5 | 10 | 15),
        "{}: ui.snap_minutes must be 5, 10, or 15 (got {})",
        path.display(),
        config.ui.snap_minutes
    );
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = load_from(Path::new("/nonexistent/suemo/config.toml")).unwrap();
        assert_eq!(cfg.ui.snap_minutes, 5);
    }

    #[test]
    fn overrides_apply_and_unknown_keys_are_ignored() {
        let dir = std::env::temp_dir().join(format!("suemo-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui]\nsnap_minutes = 15\nbogus_key = true\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.ui.snap_minutes, 15);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snap_outside_the_choices_is_a_hard_error() {
        let dir = std::env::temp_dir().join(format!("suemo-config-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui]\nsnap_minutes = 7\n").unwrap();
        assert!(load_from(&path).is_err());
        std::fs::write(&path, "[ui\nsnap_minutes = 5\n").unwrap();
        assert!(load_from(&path).is_err()); // invalid TOML is also fatal
        std::fs::remove_dir_all(&dir).ok();
    }
}
