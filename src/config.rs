use gtk4::glib;
use serde::Deserialize;
use std::collections::HashMap;
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
    pub texts: Texts,
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

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Auth {
    pub eager: bool,
    pub rearm_window: Rearm,
    /// The username shown when nothing is remembered.
    pub user: Option<String>,
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

/// Name → argv, for `button` actions and the power buttons.
#[derive(Debug)]
pub struct Commands(pub HashMap<String, Vec<String>>);

impl Default for Commands {
    fn default() -> Self {
        let argv = |a: &str, b: &str| vec![a.to_string(), b.to_string()];
        Self(HashMap::from([
            ("reboot".to_string(), argv("systemctl", "reboot")),
            ("poweroff".to_string(), argv("systemctl", "poweroff")),
        ]))
    }
}

impl Commands {
    pub fn get(&self, name: &str) -> Option<&[String]> {
        self.0.get(name).map(Vec::as_slice)
    }
}

/// A user table adds to and overrides the defaults, never removes one.
impl<'de> Deserialize<'de> for Commands {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut commands = Self::default();
        commands.0.extend(HashMap::<String, Vec<String>>::deserialize(deserializer)?);
        Ok(commands)
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
    /// Ids: `only` keeps just these, `hide` drops these, `order` lists
    /// these first (the rest follow by name).
    pub only: Vec<String>,
    pub hide: Vec<String>,
    pub order: Vec<String>,
    /// Appended to the display name of X11 sessions.
    pub x11_suffix: String,
    /// Id → display name (`[sessions.names]`).
    pub names: HashMap<String, String>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            x11_prefix: vec!["startx".into(), "/usr/bin/env".into()],
            env: vec![],
            default: None,
            only: vec![],
            hide: vec![],
            order: vec![],
            x11_suffix: " (X11)".into(),
            names: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Texts {
    pub enter_username: String,
    pub username_changed: String,
    /// When greetd's error description is empty.
    pub auth_failed: String,
    /// `{program}` and `{error}` are filled in.
    pub power_failed: String,
    pub rewrite: Vec<Rewrite>,
}

impl Default for Texts {
    fn default() -> Self {
        Self {
            enter_username: "enter a username".into(),
            username_changed: "username changed — enter the password again".into(),
            auth_failed: "authentication failed".into(),
            power_failed: "{program}: {error}".into(),
            rewrite: vec![],
        }
    }
}

/// `[[texts.rewrite]]`: a text containing `match` (a substring, or a
/// glib::Regex pattern with `regex = true`) becomes `text` — empty drops
/// it; `kind` limits the rule to one kind of text. The first match wins.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Rewrite {
    #[serde(rename = "match")]
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    pub text: String,
    #[serde(default)]
    pub kind: Option<TextKind>,
}

/// `error`: a PAM message mid-conversation; `failure`: a conversation
/// that ended in an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextKind {
    Info,
    Error,
    Prompt,
    Failure,
}

impl Texts {
    /// The text after the first matching rule; `None` when that rule
    /// drops it. An empty text (a clear) passes untouched.
    pub fn rewrite(&self, kind: TextKind, text: &str) -> Option<String> {
        if text.is_empty() {
            return Some(String::new());
        }
        let rule = self
            .rewrite
            .iter()
            .filter(|rule| rule.kind.is_none_or(|limit| limit == kind))
            .find_map(|rule| rule.apply(text));
        match rule {
            Some(rewritten) => (!rewritten.is_empty()).then_some(rewritten),
            None => Some(text.to_string()),
        }
    }

    fn problems(&self) -> Vec<String> {
        self.rewrite
            .iter()
            .enumerate()
            .filter_map(|(i, rule)| {
                let err = rule.compile()?.err()?;
                Some(format!("texts.rewrite[{i}]: `{}` is not a valid regex: {err}", rule.pattern))
            })
            .collect()
    }
}

impl Rewrite {
    fn compile(&self) -> Option<Result<glib::Regex, glib::Error>> {
        if !self.regex {
            return None;
        }
        let flags = (glib::RegexCompileFlags::DEFAULT, glib::RegexMatchFlags::DEFAULT);
        // g_regex_new returns a regex or an error, never neither.
        Some(glib::Regex::new(&self.pattern, flags.0, flags.1).map(Option::unwrap))
    }

    /// `Some` when the rule matches; a regex rule expands `\1` references
    /// in `text`.
    fn apply(&self, text: &str) -> Option<String> {
        let Some(regex) = self.compile() else {
            return text.contains(&self.pattern).then(|| self.text.clone());
        };
        let text = glib::GString::from(text);
        let found = regex.ok()?.match_(text.as_gstr(), glib::RegexMatchFlags::DEFAULT).ok()?;
        if !found.matches() {
            return None;
        }
        match found.expand_references(&self.text) {
            Ok(expanded) => Some(expanded.map(|s| s.to_string()).unwrap_or_default()),
            Err(_) => Some(self.text.clone()),
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
            problems.extend(
                config.texts.problems().iter().map(|p| format!("config {}: {p}", path.display())),
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
        assert_eq!(config.commands.get("reboot").unwrap(), ["systemctl", "reboot"]);
        assert_eq!(config.paths.style, PathBuf::from("style.css"));
    }

    #[test]
    fn a_commands_table_adds_and_overrides_but_keeps_the_rest() {
        let text = "[commands]\nsuspend = [\"systemctl\", \"suspend\"]\nreboot = [\"loginctl\", \"reboot\"]\n";
        let (config, unknown) = parse(text).unwrap();
        assert!(unknown.is_empty(), "{unknown:?}");
        assert_eq!(config.commands.get("suspend").unwrap(), ["systemctl", "suspend"]);
        assert_eq!(config.commands.get("reboot").unwrap(), ["loginctl", "reboot"]);
        assert_eq!(config.commands.get("poweroff").unwrap(), ["systemctl", "poweroff"]);
        assert_eq!(config.commands.get("hibernate"), None);
    }

    #[test]
    fn auth_user_is_the_fallback_username() {
        assert_eq!(parse("").unwrap().0.auth.user, None);
        let config = parse("[auth]\nuser = \"edraven\"\n").unwrap().0;
        assert_eq!(config.auth.user.as_deref(), Some("edraven"));
    }

    #[test]
    fn sessions_filters_names_and_suffix_parse() {
        let text = "[sessions]\nonly = [\"wayland/a\"]\nhide = [\"x11/b\"]\n\
                    order = [\"wayland/c\", \"wayland/a\"]\nx11-suffix = \"\"\n\
                    [sessions.names]\n\"wayland/a\" = \"Alpha\"\n";
        let (config, unknown) = parse(text).unwrap();
        assert!(unknown.is_empty(), "{unknown:?}");
        let sessions = &config.sessions;
        assert_eq!(sessions.only, ["wayland/a"]);
        assert_eq!(sessions.hide, ["x11/b"]);
        assert_eq!(sessions.order, ["wayland/c", "wayland/a"]);
        assert_eq!(sessions.x11_suffix, "");
        assert_eq!(sessions.names["wayland/a"], "Alpha");
        assert_eq!(Sessions::default().x11_suffix, " (X11)");
    }

    #[test]
    fn texts_default_to_the_built_in_strings() {
        let texts = parse("").unwrap().0.texts;
        assert_eq!(texts.enter_username, "enter a username");
        assert_eq!(texts.username_changed, "username changed — enter the password again");
        assert_eq!(texts.auth_failed, "authentication failed");
        assert_eq!(texts.power_failed, "{program}: {error}");
        assert!(texts.rewrite.is_empty());
        let texts = parse("[texts]\nauth-failed = \"nope\"\n").unwrap().0.texts;
        assert_eq!(texts.auth_failed, "nope");
        assert_eq!(texts.enter_username, "enter a username");
    }

    fn rules(toml: &str) -> Texts {
        parse(toml).unwrap().0.texts
    }

    #[test]
    fn rewrite_substring_first_match_wins_and_empty_drops() {
        let texts = rules(
            "[[texts.rewrite]]\nmatch = \"finger\"\ntext = \"Touch the reader\"\n\
             [[texts.rewrite]]\nmatch = \"Place\"\ntext = \"never reached\"\n\
             [[texts.rewrite]]\nmatch = \"timed out\"\ntext = \"\"\n",
        );
        let place = "Place your right thumb on the fingerprint reader";
        assert_eq!(texts.rewrite(TextKind::Info, place).as_deref(), Some("Touch the reader"));
        assert_eq!(texts.rewrite(TextKind::Info, "Verification timed out"), None);
        assert_eq!(texts.rewrite(TextKind::Info, "Finger").as_deref(), Some("Finger"));
        assert_eq!(texts.rewrite(TextKind::Info, "").as_deref(), Some(""));
    }

    #[test]
    fn rewrite_kind_limits_a_rule() {
        let texts = rules(
            "[[texts.rewrite]]\nmatch = \"Password\"\ntext = \"Passwort:\"\nkind = \"prompt\"\n",
        );
        assert_eq!(texts.rewrite(TextKind::Prompt, "Password:").as_deref(), Some("Passwort:"));
        assert_eq!(texts.rewrite(TextKind::Error, "Password:").as_deref(), Some("Password:"));
        assert_eq!(texts.rewrite(TextKind::Failure, "Password:").as_deref(), Some("Password:"));
    }

    #[test]
    fn rewrite_regex_expands_references() {
        let texts = rules(
            "[[texts.rewrite]]\nmatch = '^Place your (\\w+) (\\w+)'\nregex = true\n\
             text = 'Your \\1 \\2, please'\n",
        );
        let place = "Place your right thumb on the fingerprint reader";
        let expected = Some("Your right thumb, please");
        assert_eq!(texts.rewrite(TextKind::Info, place).as_deref(), expected);
        assert_eq!(
            texts.rewrite(TextKind::Info, "Misplace your").as_deref(),
            Some("Misplace your")
        );
    }

    #[test]
    fn a_bad_regex_is_a_problem_and_matches_nothing() {
        let texts = rules("[[texts.rewrite]]\nmatch = \"(\"\nregex = true\ntext = \"x\"\n");
        let problems = texts.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("texts.rewrite[0]: `(` is not a valid regex"),
            "{problems:?}"
        );
        assert_eq!(texts.rewrite(TextKind::Info, "(").as_deref(), Some("("));
        assert!(rules("[[texts.rewrite]]\nmatch = \"(\"\ntext = \"x\"\n").problems().is_empty());
    }

    #[test]
    fn a_rewrite_rule_needs_match_and_text_and_a_known_kind() {
        assert!(parse("[[texts.rewrite]]\ntext = \"x\"\n").is_err());
        assert!(parse("[[texts.rewrite]]\nmatch = \"x\"\n").is_err());
        assert!(
            parse("[[texts.rewrite]]\nmatch = \"x\"\ntext = \"y\"\nkind = \"warning\"\n").is_err()
        );
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
