//! Last-user / last-session cache in /var/lib/hypred-greeter/state.toml
//! (ship a tmpfiles.d entry owning it as the greeter user). Read/write
//! failures are logged and shrugged off — demo mode has no write access
//! there and that's fine.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const STATE_PATH: &str = "/var/lib/hypred-greeter/state.toml";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct State {
    pub last_user: Option<String>,
    /// username → session stem
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
    // Write-then-rename so a crash mid-write can't corrupt the cache.
    let tmp = format!("{STATE_PATH}.new");
    let result = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, STATE_PATH));
    if let Err(err) = result {
        warn_!("state save {STATE_PATH}: {err}");
    }
}
