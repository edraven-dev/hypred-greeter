use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const STATE_DIR: &str = "/var/lib/hypred-greeter";

/// One file per VT: a second greetd instance (another user's entry point)
/// must not overwrite whom this one remembers.
fn file_name(vt: Option<&str>) -> String {
    match vt {
        Some(vt) => format!("state-vt{vt}.toml"),
        None => "state.toml".into(),
    }
}

fn path() -> PathBuf {
    Path::new(STATE_DIR).join(file_name(crate::seat::vt().as_deref()))
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct State {
    pub last_user: Option<String>,
    /// username → kind-qualified session id ("wayland/hyprland-uwsm")
    pub last_session: HashMap<String, String>,
}

pub fn load() -> State {
    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
            warn_!("state {}: {err}", path.display());
            State::default()
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(err) => {
            warn_!("state {}: {err}", path.display());
            State::default()
        }
    }
}

pub fn save(state: &State) {
    let text = match toml::to_string(state) {
        Ok(text) => text,
        Err(err) => return warn_!("state serialize: {err}"),
    };
    let path = path();
    let tmp = path.with_extension("toml.new");
    let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = result {
        warn_!("state save {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_vt_has_its_own_file() {
        assert_eq!(file_name(Some("1")), "state-vt1.toml");
        assert_eq!(file_name(Some("2")), "state-vt2.toml");
        assert_eq!(file_name(None), "state.toml");
    }

    #[test]
    fn seeded_state_file_parses() {
        let text =
            "last-user = \"edraven\"\n\n[last-session]\nedraven = \"wayland/hyprland-uwsm\"\n";
        let state: State = toml::from_str(text).unwrap();
        assert_eq!(state.last_user.as_deref(), Some("edraven"));
        assert_eq!(state.last_session["edraven"], "wayland/hyprland-uwsm");
        assert_eq!(toml::to_string(&state).unwrap(), text);
    }
}
