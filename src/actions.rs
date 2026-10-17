use crate::config::{Commands, Texts};
use crate::ui::bus::{FocusTarget, UiEvent};
use crate::ui::ctx::AppHandle;

const BUILT_IN: &str = "cancel, next-session, prev-session, focus-username, focus-password";

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Cancel,
    NextSession,
    PrevSession,
    Focus(FocusTarget),
    Run(Vec<String>),
}

impl Action {
    pub fn named(name: &str, commands: &Commands) -> Result<Self, String> {
        Ok(match name {
            "cancel" => Action::Cancel,
            "next-session" => Action::NextSession,
            "prev-session" => Action::PrevSession,
            "focus-username" => Action::Focus(FocusTarget::Username),
            "focus-password" => Action::Focus(FocusTarget::Password),
            _ => match commands.get(name) {
                Some([]) => return Err(format!("command `{name}` is empty")),
                Some(argv) => Action::Run(argv.to_vec()),
                None => {
                    return Err(format!(
                        "unknown action `{name}` (built-in: {BUILT_IN}; or a [commands] name)"
                    ))
                }
            },
        })
    }

    pub fn perform(&self, app: &AppHandle, demo: bool, texts: &Texts) {
        match self {
            Action::Cancel => app.auth.cancel(),
            Action::NextSession => app.step_session(1),
            Action::PrevSession => app.step_session(-1),
            Action::Focus(target) => app.emit(&UiEvent::Focus(*target)),
            Action::Run(argv) => run(app, demo, texts, argv),
        }
    }
}

/// The one place a configured command is started from.
fn run(app: &AppHandle, demo: bool, texts: &Texts, argv: &[String]) {
    let Some((program, args)) = argv.split_first() else { return };
    if demo {
        app.emit(&UiEvent::Info(format!("demo: would run {}", argv.join(" "))));
        return;
    }
    let spawned = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .env_remove("GREETD_SOCK")
        .spawn();
    match spawned {
        // Reaped off the main thread: a dropped Child stays a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        // A PamError: an AuthError would wipe a half-typed password.
        Err(err) => {
            let text = texts
                .power_failed
                .replace("{program}", program)
                .replace("{error}", &err.to_string());
            app.emit(&UiEvent::PamError(text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn a_name_is_a_built_in_or_a_commands_entry() {
        let mut commands = Commands::default();
        commands.0.insert("empty".into(), Vec::new());
        let named = |name: &str| Action::named(name, &commands);
        assert_eq!(named("cancel").unwrap(), Action::Cancel);
        assert_eq!(named("next-session").unwrap(), Action::NextSession);
        assert_eq!(named("prev-session").unwrap(), Action::PrevSession);
        assert_eq!(named("focus-username").unwrap(), Action::Focus(FocusTarget::Username));
        assert_eq!(named("focus-password").unwrap(), Action::Focus(FocusTarget::Password));
        assert_eq!(named("poweroff").unwrap(), Action::Run(argv(&["systemctl", "poweroff"])));
        assert_eq!(named("empty").unwrap_err(), "command `empty` is empty");
        let err = named("nope").unwrap_err();
        assert!(err.starts_with("unknown action `nope` (built-in: cancel, next-session"), "{err}");
    }
}
