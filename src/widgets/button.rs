use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use crate::config::{Commands, Texts};
use crate::layout::Node;
use crate::ui::bus::{FocusTarget, UiEvent};
use crate::ui::ctx::{AppHandle, BuildCtx};
use crate::widgets::{image, WidgetDef, WidgetError};

/// A `confirm` button stays armed this long.
const CONFIRM_WINDOW: Duration = Duration::from_secs(3);

const BUILT_IN: &str = "cancel, next-session, prev-session, focus-username, focus-password";

pub struct ButtonDef;

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Cancel,
    NextSession,
    PrevSession,
    Focus(FocusTarget),
    Run(Vec<String>),
}

impl Action {
    fn parse(node: &Node, commands: &Commands) -> Result<Self, WidgetError> {
        let (action, command) = (node.props.str("action")?, node.props.str_list("command")?);
        match (action, command) {
            (Some(_), Some(_)) => {
                Err(WidgetError::Other("`action` and `command` exclude each other".into()))
            }
            (None, None) => {
                Err(WidgetError::Other("one of `action` or `command` is required".into()))
            }
            (None, Some(argv)) if argv.is_empty() => {
                Err(WidgetError::Other("`command` must not be empty".into()))
            }
            (None, Some(argv)) => Ok(Action::Run(argv)),
            (Some(name), None) => Self::named(&name, commands),
        }
    }

    fn named(name: &str, commands: &Commands) -> Result<Self, WidgetError> {
        Ok(match name {
            "cancel" => Action::Cancel,
            "next-session" => Action::NextSession,
            "prev-session" => Action::PrevSession,
            "focus-username" => Action::Focus(FocusTarget::Username),
            "focus-password" => Action::Focus(FocusTarget::Password),
            _ => match commands.get(name) {
                Some([]) => {
                    return Err(WidgetError::Other(format!("`action`: command `{name}` is empty")))
                }
                Some(argv) => Action::Run(argv.to_vec()),
                None => {
                    return Err(WidgetError::Other(format!(
                        "`action`: unknown action `{name}` (built-in: {BUILT_IN}; or a [commands] name)"
                    )))
                }
            },
        })
    }

    fn perform(&self, app: &AppHandle, demo: bool, texts: &Texts) {
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
    if let Err(err) = std::process::Command::new(program).args(args).spawn() {
        let text =
            texts.power_failed.replace("{program}", program).replace("{error}", &err.to_string());
        app.emit(&UiEvent::AuthError(text));
    }
}

impl WidgetDef for ButtonDef {
    fn kind(&self) -> &'static str {
        "button"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let action = Action::parse(node, &ctx.config.commands)?;
        let button = gtk::Button::new();
        match (node.props.str("icon")?, node.props.str("label")?) {
            (Some(icon), Some(text)) => {
                let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                content.append(&image::load_icon(&icon)?);
                content.append(&gtk::Label::new(Some(&text)));
                button.set_child(Some(&content));
            }
            (Some(icon), None) => {
                button.set_child(Some(&image::load_icon(&icon)?));
                button.add_css_class("image-button");
            }
            (None, Some(text)) => button.set_label(&text),
            (None, None) => {}
        }
        if let Some(tooltip) = node.props.str("tooltip")? {
            button.set_tooltip_text(Some(&tooltip));
        }
        let confirm = node.props.bool("confirm")?.unwrap_or(false);

        let (app, demo, texts) = (ctx.app.clone(), ctx.demo, ctx.config.texts.clone());
        let armed = Rc::new(Cell::new(false));
        // Bumped by every arm and run, so a disarm timer of an earlier arm
        // finds itself stale.
        let arms = Rc::new(Cell::new(0u64));
        button.connect_clicked(move |button| {
            if confirm && !armed.replace(true) {
                let ticket = arms.get() + 1;
                arms.set(ticket);
                button.add_css_class("hg-button-confirm");
                let (weak, armed, arms) = (button.downgrade(), armed.clone(), arms.clone());
                glib::timeout_add_local_once(CONFIRM_WINDOW, move || {
                    if arms.get() != ticket {
                        return;
                    }
                    armed.set(false);
                    if let Some(button) = weak.upgrade() {
                        button.remove_css_class("hg-button-confirm");
                    }
                });
                return;
            }
            if confirm {
                armed.set(false);
                arms.set(arms.get() + 1);
                button.remove_css_class("hg-button-confirm");
            }
            action.perform(&app, demo, &texts);
        });
        Ok(button.upcast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(props: &str) -> Node {
        let text = format!("[root]\nwidget = \"button\"\n{props}\n");
        crate::layout::parse_str(&text, &mut Vec::new()).unwrap()
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn an_action_is_built_in_a_commands_name_or_an_argv() {
        let commands = Commands::default();
        let parse = |props: &str| Action::parse(&node(props), &commands);
        assert_eq!(parse("action = \"cancel\"").unwrap(), Action::Cancel);
        assert_eq!(parse("action = \"next-session\"").unwrap(), Action::NextSession);
        assert_eq!(parse("action = \"prev-session\"").unwrap(), Action::PrevSession);
        let focus = Action::Focus(FocusTarget::Password);
        assert_eq!(parse("action = \"focus-password\"").unwrap(), focus);
        let reboot = Action::Run(argv(&["systemctl", "reboot"]));
        assert_eq!(parse("action = \"reboot\"").unwrap(), reboot);
        let suspend = Action::Run(argv(&["systemctl", "suspend"]));
        assert_eq!(parse("command = [\"systemctl\", \"suspend\"]").unwrap(), suspend);
    }

    #[test]
    fn exactly_one_of_action_or_command_and_it_must_exist() {
        let mut commands = Commands::default();
        commands.0.insert("empty".into(), Vec::new());
        for bad in [
            "",
            "action = \"nope\"",
            "action = \"empty\"",
            "action = \"cancel\"\ncommand = [\"x\"]",
            "command = []",
            "command = \"systemctl suspend\"",
        ] {
            let err = Action::parse(&node(bad), &commands).unwrap_err().to_string();
            assert!(!err.is_empty(), "{bad}");
        }
        let err = Action::parse(&node("action = \"nope\""), &commands).unwrap_err().to_string();
        assert!(err.contains("unknown action `nope`") && err.contains("next-session"), "{err}");
    }
}
