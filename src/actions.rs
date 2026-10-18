use gtk4 as gtk;
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::config::{Commands, Config, Texts};
use crate::log::{error, warn_};
use crate::ui::bus::{FocusTarget, UiEvent};
use crate::ui::ctx::AppHandle;

const BUILT_IN: &str = "cancel, next-session, prev-session, focus-username, focus-password";

/// A command that fails later than this after its click is a long-lived
/// helper dying, not an answer to the click.
const REPORT_WITHIN: Duration = Duration::from_secs(10);

/// A run of a command this soon after its last one asks for the same thing
/// again, and its failure is no news: logind refuses a second reboot while
/// the first is under way.
const REPEAT_WITHIN: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Runs(RefCell<HashMap<Vec<String>, Instant>>);

impl Runs {
    fn repeat(&self, argv: &[String], now: Instant) -> bool {
        let last = self.0.borrow_mut().insert(argv.to_vec(), now);
        last.is_some_and(|last| now.saturating_duration_since(last) < REPEAT_WITHIN)
    }
}

/// Autorepeat presses of a held key come closer together than this; a key
/// still down after a longer pause had its release go elsewhere (an open
/// list, another program).
const HELD_GAP: Duration = Duration::from_secs(2);

/// The keys that are down. Autorepeat sends more presses of a held key with
/// no release between, and GDK does not mark them.
#[derive(Default)]
struct Held {
    keys: RefCell<HashMap<u32, Instant>>,
    repeat: Cell<bool>,
}

impl Held {
    fn press(&self, keycode: u32, now: Instant) {
        let last = self.keys.borrow_mut().insert(keycode, now);
        self.repeat.set(last.is_some_and(|last| now.saturating_duration_since(last) < HELD_GAP));
    }

    fn release(&self, keycode: u32) {
        self.keys.borrow_mut().remove(&keycode);
    }

    fn repeating(&self) -> bool {
        self.repeat.get()
    }
}

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

    fn repeats(&self) -> bool {
        !matches!(self, Action::Run(_))
    }
}

fn run(app: &AppHandle, demo: bool, texts: &Texts, argv: &[String]) {
    let Some((program, args)) = argv.split_first() else { return };
    if demo {
        app.emit(&UiEvent::Info(format!("demo: would run {}", argv.join(" "))));
        return;
    }
    let repeat = app.runs.repeat(argv, Instant::now());
    let spawned =
        Command::new(program).args(args).stdin(Stdio::null()).env_remove("GREETD_SOCK").spawn();
    match spawned {
        Ok(child) => {
            let failure = reap(child, Instant::now() + REPORT_WITHIN);
            if repeat {
                return;
            }
            let (app, template, argv) = (app.clone(), texts.power_failed.clone(), argv.to_vec());
            glib::spawn_future_local(async move {
                if let Ok(why) = failure.recv().await {
                    report(&app, &template, &argv, &why);
                }
            });
        }
        Err(_) if repeat => {}
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

/// The `[keys]` table, as controllers to add to the window in this order:
/// GTK runs the one added last first, and the key watcher has to see every
/// press before a shortcut takes it. Capture phase: a bound key works
/// whatever widget has the focus, before an entry can take it as input —
/// but an open dropdown list or menu is a surface of its own, and its keys
/// never reach the window.
pub fn shortcuts(
    config: &Config,
    app: &AppHandle,
    demo: bool,
    problems: &mut Vec<String>,
) -> [gtk::EventController; 2] {
    let held = Rc::new(Held::default());
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let down = held.clone();
    keys.connect_key_pressed(move |_, _, keycode, _| {
        down.press(keycode, Instant::now());
        glib::Propagation::Proceed
    });
    let up = held.clone();
    keys.connect_key_released(move |_, _, keycode, _| up.release(keycode));

    let controller = gtk::ShortcutController::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let (bindings, found) = bind(&config.keys.0, &config.commands, |key| {
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
        let (app, texts, held) = (app.clone(), config.texts.clone(), held.clone());
        let callback = gtk::CallbackAction::new(move |_, _| {
            if action.repeats() || !held.repeating() {
                action.perform(&app, demo, &texts);
            }
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(callback)));
    }
    [controller.upcast(), keys.upcast()]
}

/// A key spelt twice (`<Ctrl>r`, `<Control>r`) keeps the first in the file.
fn bind<T: ToString>(
    keys: &[(String, String)],
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

    /// Runs each argv as a click would and returns what was shown, once
    /// `expected` PamErrors are in and a little after, for any extra one.
    fn shown_after(runs: &[&[&str]], expected: usize) -> Vec<String> {
        let shared = Shared::new(String::new(), Vec::new(), State::default(), None);
        events(shared, |app| {
            let errors = Rc::new(Cell::new(0));
            let counter = errors.clone();
            app.bus.subscribe(move |event| {
                if let UiEvent::PamError(_) = event {
                    counter.set(counter.get() + 1);
                }
            });
            let texts = Texts::default();
            for words in runs {
                run(app, false, &texts, &argv(words));
            }
            let context = glib::MainContext::ref_thread_default();
            let pump_until = |deadline: Instant, done: &dyn Fn() -> bool| {
                while !done() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                    while context.iteration(false) {}
                }
            };
            pump_until(Instant::now() + Duration::from_secs(5), &|| errors.get() >= expected);
            pump_until(Instant::now() + Duration::from_millis(300), &|| false);
        })
    }

    #[test]
    fn a_command_that_cannot_start_or_exits_unsuccessfully_is_a_pam_error() {
        let log = shown_after(&[&["true"], &["/nonexistent/prog"], &["sh", "-c", "exit 3"]], 2);
        assert_eq!(
            log,
            [
                "PamError(\"/nonexistent/prog: No such file or directory (os error 2)\")",
                "PamError(\"sh: exit status: 3\")",
            ]
        );
    }

    #[test]
    fn a_command_run_again_at_once_reports_only_its_first_run() {
        let exit = &["sh", "-c", "exit 3"][..];
        let missing = &["/nonexistent/prog"][..];
        let log = shown_after(&[exit, exit, missing, missing, exit], 2);
        assert_eq!(
            log,
            [
                "PamError(\"/nonexistent/prog: No such file or directory (os error 2)\")",
                "PamError(\"sh: exit status: 3\")",
            ]
        );
    }

    #[test]
    fn a_run_soon_after_the_last_of_its_command_is_a_repeat() {
        let runs = Runs::default();
        let (reboot, poweroff) = (argv(&["systemctl", "reboot"]), argv(&["systemctl", "poweroff"]));
        let start = Instant::now();
        let after = |ms: u64| start + Duration::from_millis(ms);
        assert!(!runs.repeat(&reboot, after(0)));
        assert!(runs.repeat(&reboot, after(150)));
        assert!(!runs.repeat(&poweroff, after(200)));
        assert!(runs.repeat(&reboot, after(2100)), "counted from the last run, not the first");
        assert!(!runs.repeat(&reboot, after(4100)));
        assert!(runs.repeat(&poweroff, after(2199)));
    }

    #[test]
    fn a_press_of_a_key_still_down_is_a_repeat() {
        let (delete, shift) = (119, 50);
        let held = Held::default();
        let start = Instant::now();
        let press = |keycode: u32, ms: u64| {
            held.press(keycode, start + Duration::from_millis(ms));
            held.repeating()
        };
        assert!(!press(delete, 0));
        assert!(press(delete, 600), "the first autorepeat");
        assert!(press(delete, 640));
        assert!(!press(shift, 650));
        assert!(press(delete, 680), "a held key keeps repeating past another press");
        held.release(delete);
        assert!(!press(delete, 900), "pressed again");
        assert!(!press(delete, 3000), "its release went elsewhere");
    }

    #[test]
    fn a_held_key_steps_sessions_but_runs_a_command_once() {
        assert!(Action::NextSession.repeats());
        assert!(Action::PrevSession.repeats());
        assert!(Action::Cancel.repeats());
        assert!(Action::Focus(FocusTarget::Password).repeats());
        assert!(!Action::Run(argv(&["systemctl", "reboot"])).repeats());
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
    /// case does not matter and `<Ctrl>` is `<Control>`.
    fn parse(key: &str) -> Result<String, &'static str> {
        if key.contains("Atl") {
            Err("unreadable")
        } else {
            Ok(key.to_lowercase().replace("<ctrl>", "<control>"))
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
        let (bindings, problems) = bind(&config.keys.0, &config.commands, parse);
        assert_eq!(
            bindings,
            [
                ("<control><alt>delete".to_string(), Action::Run(argv(&["systemctl", "reboot"]))),
                ("f2".to_string(), Action::NextSession),
                ("<super>s".to_string(), Action::Run(argv(&["systemctl", "suspend"]))),
            ]
        );
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].starts_with("keys `F3`: unknown action `nope` (built-in:"),
            "{problems:?}"
        );
        assert_eq!(problems[1], "keys `<Control><Atl>Delete`: unreadable");
    }

    #[test]
    fn a_key_bound_twice_keeps_the_first_in_the_file() {
        let config: Config = toml::from_str(
            "[keys]\n\"<Ctrl>r\" = \"cancel\"\n\"<Control>r\" = \"reboot\"\n\
             \"<control>R\" = \"next-session\"\n",
        )
        .unwrap();
        let (bindings, problems) = bind(&config.keys.0, &config.commands, parse);
        assert_eq!(bindings, [("<control>r".to_string(), Action::Cancel)]);
        assert_eq!(
            problems,
            [
                "keys `<Control>r`: the same key as `<Ctrl>r`, which wins",
                "keys `<control>R`: the same key as `<Ctrl>r`, which wins",
            ]
        );
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
