use gtk4 as gtk;
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Commands, Config, Texts};
use crate::log::{error, warn_};
use crate::ui::bus::{FocusTarget, UiEvent};
use crate::ui::ctx::AppHandle;

const BUILT_IN: &str = "cancel, next-session, prev-session, focus-username, focus-password";

/// A command that fails later than this after its click is a long-lived
/// helper dying, not an answer to the click.
const REPORT_WITHIN: Duration = Duration::from_secs(10);

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
    let spawned =
        Command::new(program).args(args).stdin(Stdio::null()).env_remove("GREETD_SOCK").spawn();
    match spawned {
        Ok(child) => {
            let failure = reap(child, Instant::now() + REPORT_WITHIN);
            let (app, template, argv) = (app.clone(), texts.power_failed.clone(), argv.to_vec());
            glib::spawn_future_local(async move {
                if let Ok(why) = failure.recv().await {
                    report(&app, &template, &argv, &why);
                }
            });
        }
        Err(err) => report(app, &texts.power_failed, argv, &err.to_string()),
    }
}

/// Waits for `child` off the main thread (a dropped Child stays a zombie)
/// and hands over how it failed, if it did before `deadline`.
fn reap(mut child: Child, deadline: Instant) -> async_channel::Receiver<String> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let Ok(status) = child.wait() else { return };
        if !status.success() && Instant::now() < deadline {
            let _ = tx.send_blocking(status.to_string());
        }
    });
    rx
}

/// A PamError: an AuthError would wipe a half-typed password.
fn report(app: &AppHandle, template: &str, argv: &[String], why: &str) {
    warn_!("command `{}`: {why}", argv.join(" "));
    let text = template.replace("{program}", &argv[0]).replace("{error}", why);
    app.emit(&UiEvent::PamError(text));
}

/// The `[keys]` table on the window. Capture phase: a bound key works
/// whatever has the focus, before an entry can take it as input.
pub fn shortcuts(
    config: &Config,
    app: &AppHandle,
    demo: bool,
    problems: &mut Vec<String>,
) -> gtk::ShortcutController {
    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let (bindings, found) = bind(&config.keys, &config.commands, |key| {
        let trigger = gtk::ShortcutTrigger::parse_string(key)
            .ok_or("not a key GTK can read (write it like \"<Control><Alt>Delete\" or \"F2\")")?;
        if types_into_entries(&trigger) {
            return Err("an entry takes it as input; add <Control>, <Alt> or <Super>");
        }
        Ok(trigger)
    });
    for problem in found {
        error!("{problem}");
        problems.push(problem);
    }
    for (trigger, action) in bindings {
        let (app, texts) = (app.clone(), config.texts.clone());
        let callback = gtk::CallbackAction::new(move |_, _| {
            action.perform(&app, demo, &texts);
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(callback)));
    }
    controller
}

/// A key spelt twice (`<Ctrl>r`, `<Control>r`) keeps the first.
fn bind<T: ToString>(
    keys: &BTreeMap<String, String>,
    commands: &Commands,
    parse: impl Fn(&str) -> Result<T, &'static str>,
) -> (Vec<(T, Action)>, Vec<String>) {
    let (mut bindings, mut problems) = (Vec::new(), Vec::new());
    let mut seen: HashMap<String, &str> = HashMap::new();
    for (key, name) in keys {
        let trigger = match parse(key) {
            Ok(trigger) => trigger,
            Err(why) => {
                problems.push(format!("keys `{key}`: {why}"));
                continue;
            }
        };
        let action = match Action::named(name, commands) {
            Ok(action) => action,
            Err(why) => {
                problems.push(format!("keys `{key}`: {why}"));
                continue;
            }
        };
        match seen.entry(trigger.to_string()) {
            Entry::Occupied(first) => {
                problems
                    .push(format!("keys `{key}`: the same key as `{}`, which wins", first.get()));
            }
            Entry::Vacant(slot) => {
                slot.insert(key);
                bindings.push((trigger, action));
            }
        }
    }
    (bindings, problems)
}

fn types_into_entries(trigger: &gtk::ShortcutTrigger) -> bool {
    if let Some(keyval) = trigger.downcast_ref::<gtk::KeyvalTrigger>() {
        return types(keyval.keyval(), keyval.modifiers());
    }
    if let Some(either) = trigger.downcast_ref::<gtk::AlternativeTrigger>() {
        return types_into_entries(&either.first()) || types_into_entries(&either.second());
    }
    false
}

/// Enter, Tab, Backspace, Delete and Escape have a character too; their
/// keypad and ISO twins mostly do not.
fn types(key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
    use gdk::Key;
    let commands = gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SUPER_MASK
        | gdk::ModifierType::HYPER_MASK
        | gdk::ModifierType::META_MASK;
    let input = key.to_unicode().is_some()
        || matches!(
            key,
            Key::KP_Enter | Key::ISO_Enter | Key::KP_Tab | Key::ISO_Left_Tab | Key::KP_Delete
        );
    input && !modifiers.intersects(commands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;
    use crate::ui::ctx::{tests::events, Shared};
    use std::cell::Cell;
    use std::rc::Rc;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn a_failed_exit_is_handed_over_until_the_deadline() {
        let reaped = |words: &[&str], within: Duration| {
            let child = Command::new(words[0]).args(&words[1..]).spawn().unwrap();
            reap(child, Instant::now() + within).recv_blocking().ok()
        };
        let failed = reaped(&["sh", "-c", "exit 3"], REPORT_WITHIN);
        assert_eq!(failed.as_deref(), Some("exit status: 3"));
        let killed = reaped(&["sh", "-c", "kill -TERM $$"], REPORT_WITHIN);
        assert_eq!(killed.as_deref(), Some("signal: 15 (SIGTERM)"));
        assert_eq!(reaped(&["true"], REPORT_WITHIN), None);
        assert_eq!(reaped(&["sh", "-c", "exit 3"], Duration::ZERO), None);
    }

    #[test]
    fn a_command_that_cannot_start_or_exits_unsuccessfully_is_a_pam_error() {
        let shared = Shared::new(String::new(), Vec::new(), State::default(), None);
        let log = events(shared, |app| {
            let errors = Rc::new(Cell::new(0));
            let counter = errors.clone();
            app.bus.subscribe(move |event| {
                if let UiEvent::PamError(_) = event {
                    counter.set(counter.get() + 1);
                }
            });
            let texts = Texts::default();
            for words in [&["true"][..], &["/nonexistent/prog"], &["sh", "-c", "exit 3"]] {
                run(app, false, &texts, &argv(words));
            }
            let context = glib::MainContext::ref_thread_default();
            let deadline = Instant::now() + Duration::from_secs(5);
            while errors.get() < 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
                while context.iteration(false) {}
            }
        });
        assert_eq!(
            log,
            [
                "PamError(\"/nonexistent/prog: No such file or directory (os error 2)\")",
                "PamError(\"sh: exit status: 3\")",
            ]
        );
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

    /// Stands in for GTK's parser, which needs a display: `Atl` is a typo,
    /// case does not matter.
    fn parse(key: &str) -> Result<String, &'static str> {
        if key.contains("Atl") {
            Err("unreadable")
        } else {
            Ok(key.to_lowercase())
        }
    }

    #[test]
    fn keys_resolve_like_button_actions_and_name_the_key_of_a_problem() {
        let config: Config = toml::from_str(
            "[commands]\nsuspend = [\"systemctl\", \"suspend\"]\n\
             [keys]\n\"<Control><Alt>Delete\" = \"reboot\"\nF2 = \"next-session\"\n\
             \"<Super>s\" = \"suspend\"\nF3 = \"nope\"\n\"<Control><Atl>Delete\" = \"reboot\"\n",
        )
        .unwrap();
        let (bindings, problems) = bind(&config.keys, &config.commands, parse);
        assert_eq!(
            bindings,
            [
                ("<control><alt>delete".to_string(), Action::Run(argv(&["systemctl", "reboot"]))),
                ("<super>s".to_string(), Action::Run(argv(&["systemctl", "suspend"]))),
                ("f2".to_string(), Action::NextSession),
            ]
        );
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert_eq!(problems[0], "keys `<Control><Atl>Delete`: unreadable");
        assert!(
            problems[1].starts_with("keys `F3`: unknown action `nope` (built-in:"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_key_bound_twice_keeps_the_first() {
        let config: Config =
            toml::from_str("[keys]\n\"<Control>r\" = \"reboot\"\n\"<control>R\" = \"cancel\"\n")
                .unwrap();
        let (bindings, problems) = bind(&config.keys, &config.commands, parse);
        let reboot = Action::Run(argv(&["systemctl", "reboot"]));
        assert_eq!(bindings, [("<control>r".to_string(), reboot)]);
        assert_eq!(problems, ["keys `<control>R`: the same key as `<Control>r`, which wins"]);
    }

    #[test]
    fn only_a_key_that_types_needs_a_modifier() {
        use gdk::{Key, ModifierType as M};
        assert!(types(Key::q, M::empty()));
        assert!(types(Key::Q, M::SHIFT_MASK));
        assert!(types(Key::space, M::empty()));
        assert!(types(Key::KP_1, M::empty()));
        for typing in [
            Key::Return,
            Key::KP_Enter,
            Key::Tab,
            Key::ISO_Left_Tab,
            Key::BackSpace,
            Key::Delete,
            Key::KP_Delete,
            Key::Escape,
        ] {
            assert!(types(typing, M::empty()), "{typing:?}");
            assert!(types(typing, M::SHIFT_MASK), "{typing:?}");
        }
        assert!(!types(Key::q, M::CONTROL_MASK));
        assert!(!types(Key::Delete, M::CONTROL_MASK | M::ALT_MASK));
        assert!(!types(Key::q, M::ALT_MASK));
        assert!(!types(Key::Return, M::SUPER_MASK));
        assert!(!types(Key::F2, M::empty()));
        assert!(!types(Key::PowerOff, M::empty()));
        assert!(!types(Key::Left, M::empty()));
    }
}
