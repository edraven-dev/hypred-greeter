use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::layout::Node;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::{AppHandle, BuildCtx};
use crate::widgets::{apply_text_props, clock, WidgetDef, WidgetError};

pub struct LabelDef;

#[derive(Debug, PartialEq, Eq)]
enum Placeholder {
    User,
    Hostname,
    Session,
    Time(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Piece {
    Text(String),
    Hole(Placeholder),
}

/// `text` with `{user}`, `{hostname}`, `{session}` and `{time:FORMAT}`;
/// `{{` and `}}` are literal braces.
#[derive(Debug, PartialEq, Eq)]
struct Template(Vec<Piece>);

impl Template {
    fn parse(text: &str) -> Result<Self, String> {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if (c == '{' || c == '}') && chars.peek() == Some(&c) {
                chars.next();
                literal.push(c);
            } else if c == '{' {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => {
                            return Err(format!("unclosed `{{{name}` (a literal brace is `{{{{`)"))
                        }
                    }
                }
                let hole = match name.as_str() {
                    "user" => Placeholder::User,
                    "hostname" => Placeholder::Hostname,
                    "session" => Placeholder::Session,
                    _ => match name.strip_prefix("time:") {
                        Some(format) => Placeholder::Time(format.to_string()),
                        None => {
                            return Err(format!(
                                "unknown placeholder `{{{name}}}` (user, hostname, session, time:FORMAT)"
                            ))
                        }
                    },
                };
                if !literal.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut literal)));
                }
                pieces.push(Piece::Hole(hole));
            } else {
                literal.push(c);
            }
        }
        if !literal.is_empty() {
            pieces.push(Piece::Text(literal));
        }
        Ok(Self(pieces))
    }

    fn time_formats(&self) -> impl Iterator<Item = &str> {
        self.0.iter().filter_map(|piece| match piece {
            Piece::Hole(Placeholder::Time(format)) => Some(format.as_str()),
            _ => None,
        })
    }

    /// `escape`: the values land in Pango markup.
    fn render(&self, value: impl Fn(&Placeholder) -> String, escape: bool) -> String {
        let mut out = String::new();
        for piece in &self.0 {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::Hole(hole) => {
                    let value = value(hole);
                    if escape {
                        out.push_str(&glib::markup_escape_text(&value));
                    } else {
                        out.push_str(&value);
                    }
                }
            }
        }
        out
    }
}

struct Live {
    label: glib::WeakRef<gtk::Label>,
    template: RefCell<Template>,
    hostname: String,
    app: AppHandle,
    markup: bool,
    ticking: Cell<bool>,
}

impl Live {
    fn render(&self) {
        let Some(label) = self.label.upgrade() else { return };
        let text = self.template.borrow().render(
            |hole| match hole {
                Placeholder::User => self.app.username(),
                Placeholder::Hostname => self.hostname.clone(),
                Placeholder::Session => self.app.session_name(),
                Placeholder::Time(format) => {
                    clock::now(format).map(|t| t.to_string()).unwrap_or_default()
                }
            },
            self.markup,
        );
        label.set_label(&text);
    }

    /// A command's output replaces the template; placeholders apply to
    /// it too.
    fn set_source(self: &Rc<Self>, text: &str) {
        match Template::parse(text) {
            Ok(template) => {
                *self.template.borrow_mut() = template;
                self.render();
                self.tick();
            }
            Err(why) => warn_!("label command output: {why}"),
        }
    }

    /// One ticker per label, started once a `{time:…}` is on show.
    fn tick(self: &Rc<Self>) {
        if self.ticking.get() || self.template.borrow().time_formats().next().is_none() {
            return;
        }
        self.ticking.set(true);
        let live = self.clone();
        glib::timeout_add_seconds_local(1, move || {
            if live.label.upgrade().is_none() {
                return glib::ControlFlow::Break;
            }
            live.render();
            glib::ControlFlow::Continue
        });
    }
}

/// Runs `argv` on a thread of its own — once, or every `interval`
/// seconds — and hands over each successful run's trimmed stdout.
fn run_command(argv: Vec<String>, interval: u64) -> async_channel::Receiver<String> {
    let (tx, rx) = async_channel::unbounded();
    std::thread::spawn(move || loop {
        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if tx.send_blocking(text).is_err() {
                    return;
                }
            }
            Ok(out) => warn_!("label command `{}`: {}", argv.join(" "), out.status),
            Err(err) => warn_!("label command `{}`: {err}", argv.join(" ")),
        }
        if interval == 0 {
            return;
        }
        std::thread::sleep(Duration::from_secs(interval));
    });
    rx
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|name| !name.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".into())
}

impl WidgetDef for LabelDef {
    fn kind(&self) -> &'static str {
        "label"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let text = node.props.str_or("text", "")?;
        let template =
            Template::parse(&text).map_err(|why| WidgetError::Other(format!("`text`: {why}")))?;
        if let Some(format) = template.time_formats().find(|f| clock::now(f).is_none()) {
            return Err(WidgetError::Other(format!("`text`: invalid strftime `{format}`")));
        }
        let command = node.props.str_list("command")?;
        let interval = node.props.int("interval")?.unwrap_or(0);
        match (&command, interval) {
            (Some(argv), _) if argv.is_empty() => {
                return Err(WidgetError::Other("`command` must not be empty".into()))
            }
            (None, n) if n != 0 => {
                return Err(WidgetError::Other("`interval` needs a `command`".into()))
            }
            (_, n) if n < 0 => return Err(WidgetError::Other("`interval` must be >= 0".into())),
            _ => {}
        }

        let label = gtk::Label::builder()
            .wrap(node.props.bool("wrap")?.unwrap_or(false))
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        // Without it a wrapping label asks for its one-line width and
        // stretches its container instead of wrapping.
        if let Some(chars) = node.props.int("max-width-chars")? {
            label.set_max_width_chars(chars as i32);
        }
        apply_text_props(&label, node)?;

        let live = Rc::new(Live {
            label: label.downgrade(),
            template: RefCell::new(template),
            hostname: hostname(),
            app: ctx.app.clone(),
            markup: label.uses_markup(),
            ticking: Cell::new(false),
        });
        live.render();
        live.tick();
        let follower = live.clone();
        ctx.bus.subscribe(move |event| match event {
            UiEvent::UsernameChanged(_) | UiEvent::SessionChanged(_) => follower.render(),
            UiEvent::Prompt { .. }
            | UiEvent::Info(_)
            | UiEvent::PamError(_)
            | UiEvent::AuthError(_)
            | UiEvent::Busy(_)
            | UiEvent::Armed(_)
            | UiEvent::Starting
            | UiEvent::Focus(_) => {}
        });
        if let Some(argv) = command {
            let outputs = run_command(argv, interval as u64);
            glib::spawn_future_local(async move {
                while let Ok(output) = outputs.recv().await {
                    live.set_source(&output);
                }
            });
        }
        Ok(label.upcast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(hole: &Placeholder) -> String {
        match hole {
            Placeholder::User => "alice".into(),
            Placeholder::Hostname => "jarilo".into(),
            Placeholder::Session => "Hyprland <uwsm>".into(),
            Placeholder::Time(format) => format!("time({format})"),
        }
    }

    #[test]
    fn placeholders_render_and_braces_escape() {
        let template = Template::parse("Welcome back, {user} on {hostname} {{{session}}}").unwrap();
        let expected = "Welcome back, alice on jarilo {Hyprland <uwsm>}";
        assert_eq!(template.render(fixed, false), expected);
        let expected = "Welcome back, alice on jarilo {Hyprland &lt;uwsm&gt;}";
        assert_eq!(template.render(fixed, true), expected);
        assert_eq!(Template::parse("plain").unwrap().render(fixed, true), "plain");
        assert_eq!(Template::parse("").unwrap(), Template(vec![]));
    }

    #[test]
    fn time_takes_a_format_and_is_what_ticks() {
        let template = Template::parse("{time:%H:%M} on {time:%A}").unwrap();
        assert_eq!(template.render(fixed, false), "time(%H:%M) on time(%A)");
        assert_eq!(template.time_formats().collect::<Vec<_>>(), ["%H:%M", "%A"]);
        assert!(Template::parse("{user}").unwrap().time_formats().next().is_none());
    }

    #[test]
    fn bad_placeholders_are_errors() {
        let err = Template::parse("hello {name}").unwrap_err();
        assert!(err.contains("unknown placeholder `{name}`"), "{err}");
        let err = Template::parse("hello {user").unwrap_err();
        assert!(err.contains("unclosed `{user`"), "{err}");
        assert!(Template::parse("{time}").is_err());
    }

    #[test]
    fn a_command_hands_over_its_trimmed_stdout_once() {
        let outputs = run_command(vec!["sh".into(), "-c".into(), "echo '  hi {user} '".into()], 0);
        assert_eq!(outputs.recv_blocking().unwrap(), "hi {user}");
        assert!(outputs.recv_blocking().is_err());
        let outputs = run_command(vec!["false".into()], 0);
        assert!(outputs.recv_blocking().is_err());
    }
}
