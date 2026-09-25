pub mod bus;
pub mod ctx;

use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::config;
use crate::ui::bus::{Bus, UiEvent};

pub const DEFAULT_STYLE: &str = include_str!("../../data/style.css");
/// Past this many CSS problems the banner only counts the rest.
const CSS_PROBLEM_LINES: usize = 5;
/// `.hg-failed` stays at least this long: the next input re-arms the reader
/// and pam_fprintd's next "Place your finger" follows a mismatch within one
/// round trip, both too quick for a stylesheet's animation to show.
const FAILED_HOLD: Duration = Duration::from_millis(1500);

pub fn window_content(problems: &[String], root: gtk::Widget) -> gtk::Widget {
    root.set_hexpand(true);
    root.set_vexpand(true);
    if problems.is_empty() {
        return root;
    }
    let banner = gtk::Label::builder().label(problems.join("\n")).wrap(true).xalign(0.0).build();
    banner.add_css_class("hg-banner");
    banner.set_widget_name("hg-banner");
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("hg-root-column");
    column.set_widget_name("hg-root-column");
    column.append(&banner);
    column.append(&root);
    column.upcast()
}

/// USER priority so the stylesheet wins over theme/application styles.
pub fn load_css(path: &Path) -> Vec<String> {
    let provider = gtk::CssProvider::new();
    // The callback runs inside load_from_path: the list is complete once
    // that returns.
    let errors = Rc::new(RefCell::new(Vec::new()));
    let sink = errors.clone();
    provider.connect_parsing_error(move |_, section, err| {
        let file = section.file().and_then(|file| file.basename());
        let at = section.start_location();
        let problem = css_problem(file.as_deref(), at.lines(), at.line_chars(), err);
        // A warning (a deprecation) still renders; only errors reach the banner.
        if err.kind::<gtk::CssParserWarning>().is_some() {
            warn_!("css {problem}");
            return;
        }
        error!("css {problem}");
        sink.borrow_mut().push(problem);
    });

    let mut problems = Vec::new();
    // Probe with a read so unreadable degrades like missing; on success
    // load_from_path so relative url() refs resolve against the file's dir.
    match std::fs::read_to_string(path) {
        Ok(_) => {
            info!("style: {}", path.display());
            provider.load_from_path(path);
        }
        Err(err) => {
            problems.push(format!("style {}: {err} — using built-in style", path.display()));
            provider.load_from_string(DEFAULT_STYLE);
        }
    }
    problems.extend(capped(errors.take(), CSS_PROBLEM_LINES));

    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("no display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );
    problems
}

/// `style.css:3:6: …` — GTK counts lines and columns from zero.
fn css_problem(
    file: Option<&Path>,
    line: usize,
    column: usize,
    err: &dyn std::fmt::Display,
) -> String {
    let file = file.map_or("style".into(), |file| file.to_string_lossy().into_owned());
    format!("{file}:{}:{}: {err}", line + 1, column + 1)
}

fn capped(mut lines: Vec<String>, limit: usize) -> Vec<String> {
    if lines.len() > limit {
        let more = lines.len() - limit;
        lines.truncate(limit);
        lines.push(format!("css: +{more} more"));
    }
    lines
}

/// Window state classes: any descendant can react (`.hg-failed #card {…}`).
pub fn track_states(window: &gtk::ApplicationWindow, bus: &Bus) {
    let weak = window.downgrade();
    let failed = Rc::new(Failed::default());
    bus.subscribe(move |event| {
        let Some(window) = weak.upgrade() else { return };
        for (class, on) in state_changes(event) {
            match (class, on) {
                ("hg-failed", true) => failed.set(&window),
                ("hg-failed", false) => failed.clear(&window),
                _ => toggle_class(&window, class, on),
            }
        }
    });

    let display = WidgetExt::display(window);
    let Some(keyboard) = display.default_seat().and_then(|seat| seat.keyboard()) else {
        warn_!("no keyboard on the seat: .hg-caps-lock is not tracked");
        return;
    };
    let weak = window.downgrade();
    let caps_lock = move |keyboard: &gtk::gdk::Device| {
        if let Some(window) = weak.upgrade() {
            toggle_class(&window, "hg-caps-lock", keyboard.is_caps_locked());
        }
    };
    caps_lock(&keyboard);
    keyboard.connect_caps_lock_state_notify(caps_lock);
}

/// `.hg-failed` with its hold; a failure while it is still on drops it for
/// a frame so a CSS animation restarts (see message.rs).
#[derive(Default)]
struct Failed {
    since: Cell<Option<Instant>>,
    /// Bumped by every failure: a timer or tick from before is stale.
    ticket: Cell<u64>,
}

impl Failed {
    fn set(self: &Rc<Self>, window: &gtk::ApplicationWindow) {
        let repeated = window.has_css_class("hg-failed");
        self.since.set(Some(Instant::now()));
        let ticket = self.ticket.get() + 1;
        self.ticket.set(ticket);
        if !repeated {
            window.add_css_class("hg-failed");
            return;
        }
        window.remove_css_class("hg-failed");
        let (me, ticks) = (self.clone(), Cell::new(0));
        window.add_tick_callback(move |window, _| {
            ticks.set(ticks.get() + 1);
            if ticks.get() < 2 {
                return glib::ControlFlow::Continue;
            }
            if me.ticket.get() == ticket {
                window.add_css_class("hg-failed");
            }
            glib::ControlFlow::Break
        });
    }

    fn clear(self: &Rc<Self>, window: &gtk::ApplicationWindow) {
        let Some(since) = self.since.get() else { return };
        let left = FAILED_HOLD.saturating_sub(since.elapsed());
        if left.is_zero() {
            self.since.set(None);
            window.remove_css_class("hg-failed");
            return;
        }
        let (me, weak, ticket) = (self.clone(), window.downgrade(), self.ticket.get());
        glib::timeout_add_local_once(left, move || {
            if me.ticket.get() != ticket {
                return;
            }
            me.since.set(None);
            if let Some(window) = weak.upgrade() {
                window.remove_css_class("hg-failed");
            }
        });
    }
}

fn toggle_class(widget: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// `.hg-failed` goes before it can come back (consecutive failures always
/// have a re-arm, a new submission or pam_fprintd's next "Place your
/// finger" in between), so a CSS animation on it restarts on every failure.
fn state_changes(event: &UiEvent) -> Vec<(&'static str, bool)> {
    match event {
        UiEvent::Busy(true) => vec![("hg-busy", true), ("hg-failed", false)],
        UiEvent::Busy(false) => vec![("hg-busy", false), ("hg-starting", false)],
        UiEvent::AuthError(_) | UiEvent::PamError(_) => {
            vec![("hg-failed", true), ("hg-info", false)]
        }
        UiEvent::Prompt { .. } => vec![("hg-failed", false), ("hg-info", false)],
        UiEvent::Info(text) => vec![("hg-failed", false), ("hg-info", !text.is_empty())],
        UiEvent::Armed(true) => vec![("hg-armed", true), ("hg-failed", false)],
        UiEvent::Armed(false) => vec![("hg-armed", false)],
        UiEvent::Starting => vec![("hg-starting", true)],
        UiEvent::SessionChanged(_) | UiEvent::Focus(_) => Vec::new(),
    }
}

/// GTK CSS has no viewport units, so nothing in a stylesheet could be sized
/// relative to the screen. The window's size is published as custom
/// properties instead — `--hg-vw`/`--hg-vh` (1 % of its width/height),
/// `--hg-vmin`/`--hg-vmax` — kept current as it changes:
/// `#card { min-width: calc(var(--hg-vw) * 25); }`. Below USER priority, so
/// a stylesheet may even set its own (vmin/vmax follow, they are derived
/// in CSS). Never larger than the monitor: a windowed demo whose content
/// asks for more than the window would otherwise grow without end. The
/// frame right after a resize is laid out with the previous values.
pub fn publish_viewport(window: &gtk::ApplicationWindow) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&viewport_css(0, 0));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("no display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    window.connect_realize(move |window| {
        let Some(surface) = window.surface() else { return };
        let provider = provider.clone();
        let update = move |surface: &gtk::gdk::Surface| {
            let (mut width, mut height) = (surface.width(), surface.height());
            if let Some(monitor) = surface.display().monitor_at_surface(surface) {
                let screen = monitor.geometry();
                (width, height) = (width.min(screen.width()), height.min(screen.height()));
            }
            provider.load_from_string(&viewport_css(width, height));
        };
        update(&surface);
        surface.connect_width_notify(update.clone());
        surface.connect_height_notify(update);
    });
}

fn viewport_css(width: i32, height: i32) -> String {
    let (vw, vh) = (f64::from(width.max(0)) / 100.0, f64::from(height.max(0)) / 100.0);
    format!(
        "window.hg-window {{ --hg-vw: {vw}px; --hg-vh: {vh}px; \
         --hg-vmin: min(var(--hg-vw), var(--hg-vh)); --hg-vmax: max(var(--hg-vw), var(--hg-vh)); }}"
    )
}

pub fn apply_gtk_settings(cfg: &config::Gtk) {
    let Some(settings) = gtk::Settings::default() else {
        error!("no gtk settings on default display");
        return;
    };
    if let Some(dark) = cfg.dark {
        settings.set_gtk_application_prefer_dark_theme(dark);
    }
    if let Some(theme) = &cfg.theme {
        settings.set_gtk_theme_name(Some(theme));
        // There is no Empty-dark; GTK would silently fall back to
        // Default-dark and the blank slate would not be one.
        if theme == "Empty" {
            settings.set_gtk_application_prefer_dark_theme(false);
        }
    }
    if let Some(icons) = &cfg.icon_theme {
        settings.set_gtk_icon_theme_name(Some(icons));
    }
    if let Some(cursor) = &cfg.cursor_theme {
        settings.set_gtk_cursor_theme_name(Some(cursor));
    }
    if let Some(font) = &cfg.font {
        settings.set_gtk_font_name(Some(font));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::bus::FocusTarget;

    #[test]
    fn viewport_units_are_a_hundredth_of_the_window() {
        let css = viewport_css(1800, 1125);
        assert!(css.starts_with("window.hg-window { --hg-vw: 18px; --hg-vh: 11.25px; "), "{css}");
        assert!(css.contains("--hg-vmin: min(var(--hg-vw), var(--hg-vh));"), "{css}");
        assert!(css.contains("--hg-vmax: max(var(--hg-vw), var(--hg-vh));"), "{css}");
        assert!(viewport_css(0, -4).contains("--hg-vw: 0px; --hg-vh: 0px;"));
    }

    #[test]
    fn css_problems_name_file_line_and_column_from_one() {
        let line = css_problem(Some(Path::new("style.css")), 2, 5, &"junk at end of value");
        assert_eq!(line, "style.css:3:6: junk at end of value");
        assert_eq!(css_problem(None, 0, 0, &"x"), "style:1:1: x");
    }

    #[test]
    fn the_banner_counts_css_problems_past_the_cap() {
        let lines: Vec<String> = (1..=7).map(|n| format!("style.css:{n}:1: bad")).collect();
        let shown = capped(lines.clone(), 5);
        assert_eq!(shown.len(), 6);
        assert_eq!(shown[..5], lines[..5]);
        assert_eq!(shown[5], "css: +2 more");
        assert_eq!(capped(lines[..5].to_vec(), 5), lines[..5]);
    }

    #[test]
    fn state_classes_follow_the_events() {
        let on = |event: &UiEvent, class: &str| {
            state_changes(event).iter().find(|(c, _)| *c == class).map(|(_, on)| *on)
        };
        let failed = UiEvent::AuthError("no".into());
        assert_eq!(on(&failed, "hg-failed"), Some(true));
        assert_eq!(on(&failed, "hg-info"), Some(false));
        assert_eq!(on(&UiEvent::PamError("no".into()), "hg-failed"), Some(true));
        for clears in [
            UiEvent::Prompt { secret: true, text: "Password:".into(), passive: false },
            UiEvent::Info("Place your finger".into()),
            UiEvent::Armed(true),
            UiEvent::Busy(true),
        ] {
            assert_eq!(on(&clears, "hg-failed"), Some(false), "{clears:?}");
        }
        assert_eq!(on(&UiEvent::Busy(false), "hg-failed"), None);
        assert_eq!(on(&UiEvent::Armed(false), "hg-failed"), None);

        assert_eq!(on(&UiEvent::Info("Place your finger".into()), "hg-info"), Some(true));
        assert_eq!(on(&UiEvent::Info(String::new()), "hg-info"), Some(false));
        assert_eq!(on(&UiEvent::Busy(true), "hg-busy"), Some(true));
        assert_eq!(on(&UiEvent::Busy(false), "hg-busy"), Some(false));
        assert_eq!(on(&UiEvent::Armed(true), "hg-armed"), Some(true));
        assert_eq!(on(&UiEvent::Armed(false), "hg-armed"), Some(false));
        assert_eq!(on(&UiEvent::Starting, "hg-starting"), Some(true));
        assert_eq!(on(&UiEvent::Busy(false), "hg-starting"), Some(false));
        assert!(state_changes(&UiEvent::Focus(FocusTarget::Password)).is_empty());
        assert!(state_changes(&UiEvent::SessionChanged(1)).is_empty());
    }
}
