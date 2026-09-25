use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/greetd/hypred-greeter/config.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    pub paths: Paths,
    pub background: Background,
    pub gtk: Gtk,
    pub auth: Auth,
    pub commands: Commands,
    pub sessions: Sessions,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Paths {
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
    pub fn parse(value: &str) -> Option<Self> {
        toml::Value::String(value.to_string()).try_into().ok()
    }

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
pub struct Auth {
    pub eager: bool,
    pub rearm_window: Rearm,
    /// Seconds text left in the password entry keeps a parked prompt ready
    /// after the last input — a stray key must not switch the reader off.
    pub typing_hold: u64,
    /// How long the username entry has to settle before an eager
    /// conversation opens for the name typed.
    pub username_debounce_ms: u64,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            eager: false,
            rearm_window: Rearm::Never,
            typing_hold: 30,
            username_debounce_ms: 400,
        }
    }
}

/// `rearm-window`: seconds after the last input (0 = never), or "always".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Rearm {
    #[default]
    Never,
    Window(u64),
    Always,
}

impl<'de> Deserialize<'de> for Rearm {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = Rearm;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("seconds (0 = never) or \"always\"")
            }

            fn visit_i64<E: serde::de::Error>(self, seconds: i64) -> Result<Rearm, E> {
                match u64::try_from(seconds) {
                    Ok(0) => Ok(Rearm::Never),
                    Ok(seconds) => Ok(Rearm::Window(seconds)),
                    Err(_) => Err(E::invalid_value(serde::de::Unexpected::Signed(seconds), &self)),
                }
            }

            fn visit_u64<E: serde::de::Error>(self, seconds: u64) -> Result<Rearm, E> {
                Ok(if seconds == 0 { Rearm::Never } else { Rearm::Window(seconds) })
            }

            fn visit_str<E: serde::de::Error>(self, word: &str) -> Result<Rearm, E> {
                match word {
                    "always" => Ok(Rearm::Always),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(word), &self)),
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
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
    pub x11_prefix: Vec<String>,
    pub env: Vec<String>,
    /// Session id ("wayland/hyprland-uwsm") preselected for a user with
    /// nothing remembered; else the first by name.
    pub default: Option<String>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            x11_prefix: vec!["startx".into(), "/usr/bin/env".into()],
            env: vec![],
            default: None,
        }
    }
}

pub struct Loaded {
    pub config: Config,
    pub base_dir: PathBuf,
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
        Ok((config, unknown)) => {
            // A misspelt key silently leaves its default in force
            // (`rearm_window` would switch re-arming off): say so on screen.
            problems.extend(
                unknown.iter().map(|key| format!("config {}: unknown key `{key}`", path.display())),
            );
            config
        }
        Err(err) => {
            problems.push(format!("config {}: {err}", path.display()));
            Config::default()
        }
    };
    Loaded { config, base_dir, problems }
}

fn parse(text: &str) -> Result<(Config, Vec<String>), toml::de::Error> {
    let table: toml::Table = text.parse()?;
    let mut unknown = Vec::new();
    let config = serde_ignored::deserialize(toml::Value::Table(table), |key| {
        unknown.push(key.to_string());
    })?;
    Ok((config, unknown))
}

impl Loaded {
    pub fn resolve(&self, cli_override: Option<&Path>, configured: &Path) -> PathBuf {
        let path = cli_override.unwrap_or(configured);
        if path.is_absolute() {
            path.into()
        } else {
            self.base_dir.join(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_defaults() {
        let config = parse("").unwrap().0;
        assert_eq!(config.commands.reboot, ["systemctl", "reboot"]);
        assert_eq!(config.paths.style, PathBuf::from("style.css"));
    }

    #[test]
    fn kebab_keys_parse() {
        let config = parse(
            "[gtk]\ndark = false\nicon-theme = \"Papirus\"\n[background]\nfit = \"scale-down\"\n",
        )
        .unwrap()
        .0;
        assert_eq!(config.gtk.dark, Some(false));
        assert_eq!(config.gtk.icon_theme.as_deref(), Some("Papirus"));
        assert!(matches!(config.background.fit, Fit::ScaleDown));
    }

    #[test]
    fn auth_section_defaults_off_and_parses() {
        let config = parse("").unwrap().0;
        assert!(!config.auth.eager);
        assert_eq!(config.auth.rearm_window, Rearm::Never);
        let config = parse("[auth]\neager = true\nrearm-window = 120\n").unwrap().0;
        assert!(config.auth.eager);
        assert_eq!(config.auth.rearm_window, Rearm::Window(120));
    }

    #[test]
    fn auth_timings_default_and_parse() {
        let config = parse("").unwrap().0;
        assert_eq!(config.auth.typing_hold, 30);
        assert_eq!(config.auth.username_debounce_ms, 400);
        let config = parse("[auth]\ntyping-hold = 5\nusername-debounce-ms = 0\n").unwrap().0;
        assert_eq!(config.auth.typing_hold, 5);
        assert_eq!(config.auth.username_debounce_ms, 0);
        assert!(parse("[auth]\ntyping-hold = -1\n").is_err());
    }

    #[test]
    fn rearm_window_takes_seconds_or_always() {
        let rearm = |text: &str| parse(text).map(|(config, _)| config.auth.rearm_window);
        assert_eq!(rearm("[auth]\nrearm-window = 0\n").unwrap(), Rearm::Never);
        assert_eq!(rearm("[auth]\nrearm-window = \"always\"\n").unwrap(), Rearm::Always);
        let err = rearm("[auth]\nrearm-window = \"forever\"\n").unwrap_err().to_string();
        assert!(err.contains("always"), "{err}");
        assert!(rearm("[auth]\nrearm-window = -5\n").is_err());
    }

    #[test]
    fn unknown_keys_are_reported() {
        let (config, unknown) = parse("[auth]\neager = true\nrearm_window = \"always\"\n").unwrap();
        assert!(config.auth.eager);
        assert_eq!(config.auth.rearm_window, Rearm::Never);
        assert_eq!(unknown, ["auth.rearm_window"]);
    }

    #[test]
    fn sessions_default_parses() {
        let (config, _) = parse("[sessions]\ndefault = \"wayland/hyprland-uwsm\"\n").unwrap();
        assert_eq!(config.sessions.default.as_deref(), Some("wayland/hyprland-uwsm"));
    }

    #[test]
    fn bad_types_error_with_context() {
        let err = parse("[commands]\nreboot = \"systemctl reboot\"\n").unwrap_err();
        assert!(err.to_string().contains("reboot"));
    }
}
