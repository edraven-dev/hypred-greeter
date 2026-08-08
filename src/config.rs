//! Runtime configuration. A broken config must never brick login: every
//! failure here degrades to defaults and a recorded problem string that the
//! UI surfaces as a banner. Unknown keys warn (typo detection) but load.

use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::log::{info, warn_};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/greetd/hypred-greeter/config.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    pub paths: Paths,
    pub background: Background,
    pub gtk: Gtk,
    pub commands: Commands,
    pub sessions: Sessions,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Paths {
    /// Relative paths resolve against the config file's directory.
    pub layout: PathBuf,
    pub style: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self { layout: "layout.toml".into(), style: "style.css".into() }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Background {
    /// Default image for `background` widgets that don't set their own.
    pub image: Option<PathBuf>,
    pub fit: Fit,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fit {
    #[default]
    Cover,
    Contain,
    Fill,
    ScaleDown,
}

impl Fit {
    pub fn to_gtk(self) -> gtk4::ContentFit {
        match self {
            Fit::Cover => gtk4::ContentFit::Cover,
            Fit::Contain => gtk4::ContentFit::Contain,
            Fit::Fill => gtk4::ContentFit::Fill,
            Fit::ScaleDown => gtk4::ContentFit::ScaleDown,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Gtk {
    pub dark: Option<bool>,
    pub theme: Option<String>,
    pub icon_theme: Option<String>,
    pub cursor_theme: Option<String>,
    pub font: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Commands {
    pub reboot: Vec<String>,
    pub poweroff: Vec<String>,
}

impl Default for Commands {
    fn default() -> Self {
        Self {
            reboot: vec!["systemctl".into(), "reboot".into()],
            poweroff: vec!["systemctl".into(), "poweroff".into()],
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Sessions {
    /// Prepended to the Exec of X11 sessions (from xsessions/ dirs).
    pub x11_prefix: Vec<String>,
    /// Extra KEY=value pairs passed to greetd's start_session.
    pub env: Vec<String>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self { x11_prefix: vec!["startx".into(), "/usr/bin/env".into()], env: vec![] }
    }
}

/// A loaded config plus everything the UI needs to explain what went wrong.
pub struct Loaded {
    pub config: Config,
    /// Directory that [paths] entries resolve against.
    pub base_dir: PathBuf,
    /// Human-readable problems to surface as a banner. Empty = clean load.
    pub problems: Vec<String>,
}

pub fn load(cli_path: Option<&Path>) -> Loaded {
    let path = cli_path.map(Path::to_path_buf).unwrap_or_else(|| DEFAULT_CONFIG_PATH.into());
    let base_dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    let mut problems = Vec::new();

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && cli_path.is_none() => {
            info!("no config at {}, using defaults", path.display());
            return Loaded { config: Config::default(), base_dir, problems };
        }
        Err(err) => {
            problems.push(format!("config {}: {err}", path.display()));
            return Loaded { config: Config::default(), base_dir, problems };
        }
    };

    let config = match parse(&text) {
        Ok(config) => config,
        Err(err) => {
            problems.push(format!("config {}: {err}", path.display()));
            Config::default()
        }
    };
    Loaded { config, base_dir, problems }
}

fn parse(text: &str) -> Result<Config, toml::de::Error> {
    let table: toml::Table = text.parse()?;
    serde_ignored::deserialize(toml::Value::Table(table), |key| {
        warn_!("config: unknown key `{key}`")
    })
}

impl Loaded {
    /// Resolve a [paths] entry: CLI flag beats config, relative paths are
    /// anchored at the config directory.
    pub fn resolve(&self, cli_override: Option<&Path>, configured: &Path) -> PathBuf {
        let path = cli_override.unwrap_or(configured);
        if path.is_absolute() { path.into() } else { self.base_dir.join(path) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_defaults() {
        let config = parse("").unwrap();
        assert_eq!(config.commands.reboot, ["systemctl", "reboot"]);
        assert_eq!(config.paths.style, PathBuf::from("style.css"));
    }

    #[test]
    fn kebab_keys_parse() {
        let config = parse(
            "[gtk]\ndark = false\nicon-theme = \"Papirus\"\n[background]\nfit = \"scale-down\"\n",
        )
        .unwrap();
        assert_eq!(config.gtk.dark, Some(false));
        assert_eq!(config.gtk.icon_theme.as_deref(), Some("Papirus"));
        assert!(matches!(config.background.fit, Fit::ScaleDown));
    }

    #[test]
    fn bad_types_error_with_context() {
        let err = parse("[commands]\nreboot = \"systemctl reboot\"\n").unwrap_err();
        assert!(err.to_string().contains("reboot"));
    }
}
