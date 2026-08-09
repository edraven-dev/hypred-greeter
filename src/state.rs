use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const STATE_PATH: &str = "/var/lib/hypred-greeter/state.toml";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct State {
    pub last_user: Option<String>,
    /// username → kind-qualified session id ("wayland/hyprland-uwsm")
    pub last_session: HashMap<String, String>,
}

pub fn load() -> State {
    let path = Path::new(STATE_PATH);
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
            warn_!("state {STATE_PATH}: {err}");
            State::default()
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(err) => {
            warn_!("state {STATE_PATH}: {err}");
            State::default()
        }
    }
}

pub fn save(state: &State) {
    let text = match toml::to_string(state) {
        Ok(text) => text,
        Err(err) => return warn_!("state serialize: {err}"),
    };
    let tmp = format!("{STATE_PATH}.new");
    let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, STATE_PATH));
    if let Err(err) = result {
        warn_!("state save {STATE_PATH}: {err}");
    }
}
