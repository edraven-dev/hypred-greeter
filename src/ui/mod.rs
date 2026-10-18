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
use crate::ui::bus::UiEvent;
use crate::ui::ctx::AppHandle;

pub const DEFAULT_STYLE: &str = include_str!("../../data/style.css");
const COLOR_SCHEME: &str = "gtk-interface-color-scheme";
const PREFERS_COLOR_SCHEME: &str = "prefers-color-scheme";
/// GtkSettings' interface preferences (GTK 4.20; reduced motion 4.22) and
/// the media features a stylesheet tests them by.
const MEDIA_FEATURES: [(&str, &str); 3] = [
    (COLOR_SCHEME, PREFERS_COLOR_SCHEME),
    ("gtk-interface-contrast", "prefers-contrast"),
    ("gtk-interface-reduced-motion", "prefers-reduced-motion"),
];
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
pub fn load_css(path: &Path, cfg: &config::Gtk) -> Vec<String> {
    let display = gtk::gdk::Display::default().expect("no display");
    let provider = gtk::CssProvider::new();
    follow_settings(&gtk::Settings::for_display(&display), &provider, sheet_scheme(cfg));
    // The callback runs inside load_from_path: the list is complete once
    // that returns. A re-parse after a scheme change only logs.
    let errors = Rc::new(RefCell::new(Some(Vec::new())));
    let sink = errors.clone();
    provider.connect_parsing_error(move |_, section, err| {
        let file = section.file().and_then(|file| file.basename());
        let at = section.start_location();
        let problem = css_problem(file.as_deref(), at.lines(), at.line_chars(), err);
        if !on_banner(err) {
            warn_!("css {problem}");
            return;
        }
        error!("css {problem}");
        if let Some(sink) = sink.borrow_mut().as_mut() {
            sink.push(problem);
        }
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
    problems.extend(errors.take().unwrap_or_default());

    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );
    problems
}

/// GTK binds its settings to the media features of the theme and of the
/// user's gtk.css only; a provider of our own left unbound never matches
/// `@media (prefers-color-scheme: dark)` and the like. Bound before the
/// load, so the sheet is parsed once.
fn follow_settings(settings: &gtk::Settings, provider: &gtk::CssProvider, scheme: Option<bool>) {
    for (setting, feature) in MEDIA_FEATURES {
        let Some(pspec) = provider.find_property(feature) else { continue };
        let own = scheme.filter(|_| feature == PREFERS_COLOR_SCHEME);
        if let Some(value) = own.and_then(|dark| scheme_value(&pspec, dark)) {
            provider.set_property_from_value(feature, &value);
        } else if settings.find_property(setting).is_some() {
            settings.bind_property(setting, provider, feature).sync_create().build();
        }
    }
}

/// Under Empty GTK's own scheme stays light (see dark_scheme); the
/// stylesheet still sees `dark`.
fn sheet_scheme(cfg: &config::Gtk) -> Option<bool> {
    (cfg.theme.as_deref() == Some("Empty")).then_some(cfg.dark == Some(true))
}

/// Syntax warnings GTK raises for what CSS allows, and applies anyway: a
/// block's last declaration without `;`, an empty one (`;;`). GTK does not
/// translate them.
const CSS_RECOVERIES: [&str; 2] = ["Expected ';' at end of block", "Empty declaration"];

/// A deprecated construct still renders, and so does what GTK recovers
/// from above. Any other warning is a block that never applies (an
/// undefined `@media` feature) or a file that ends inside a block.
fn on_banner(err: &glib::Error) -> bool {
    match err.kind::<gtk::CssParserWarning>() {
        Some(gtk::CssParserWarning::Deprecated) => false,
        Some(gtk::CssParserWarning::Syntax) => !CSS_RECOVERIES.contains(&err.message()),
        _ => true,
    }
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

/// --check lists every CSS problem the banner counts past its first few.
pub fn css_lines(problems: Vec<String>, check: bool) -> Vec<String> {
    if check {
        problems
    } else {
        capped(problems, CSS_PROBLEM_LINES)
    }
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
pub fn track_states(window: &gtk::ApplicationWindow, app: &AppHandle) {
    let failed = Rc::new(Failed::default());
    let (weak, clearing) = (window.downgrade(), failed.clone());
    app.connect_clear_failed(move || {
        if let Some(window) = weak.upgrade() {
            clearing.clear(&window);
        }
    });
    let weak = window.downgrade();
    app.bus.subscribe(move |event| {
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

/// `.hg-failed` with its hold. Every failure takes the class off and puts
/// it back on the second frame, so a CSS animation restarts even when a
/// clear took it off earlier in the same frame: GTK would otherwise merge
/// the two changes (see message.rs).
#[derive(Default)]
struct Failed {
    state: RefCell<FailedState>,
}

impl Failed {
    fn set(self: &Rc<Self>, window: &gtk::ApplicationWindow) {
        let ticket = self.state.borrow_mut().fail(Instant::now());
        window.remove_css_class("hg-failed");
        let (me, ticks) = (self.clone(), Cell::new(0));
        window.add_tick_callback(move |window, _| {
            ticks.set(ticks.get() + 1);
            if ticks.get() < 2 {
                return glib::ControlFlow::Continue;
            }
            if me.state.borrow().current(ticket) {
                window.add_css_class("hg-failed");
            }
            glib::ControlFlow::Break
        });
    }

    fn clear(self: &Rc<Self>, window: &gtk::ApplicationWindow) {
        let step = self.state.borrow_mut().clear(Instant::now());
        match step {
            Clear::Nothing => {}
            Clear::Now => window.remove_css_class("hg-failed"),
            Clear::After(left, ticket) => {
                let (me, weak) = (self.clone(), window.downgrade());
                glib::timeout_add_local_once(left, move || {
                    if me.state.borrow_mut().expire(ticket) {
                        if let Some(window) = weak.upgrade() {
                            window.remove_css_class("hg-failed");
                        }
                    }
                });
            }
        }
    }
}

/// What [`Failed`] does, kept apart from GTK to be testable.
#[derive(Default)]
struct FailedState {
    since: Option<Instant>,
    /// Bumped by every failure: a timer or tick from before is stale.
    ticket: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum Clear {
    Nothing,
    Now,
    /// When the hold is over, unless another failure came meanwhile.
    After(Duration, u64),
}

impl FailedState {
    fn fail(&mut self, now: Instant) -> u64 {
        self.since = Some(now);
        self.ticket += 1;
        self.ticket
    }

    /// Whether the class may come back for `ticket`. Ticks wait for frames
    /// and a window nobody presents gets none, so the hold can run out and
    /// clear before the re-add of a repeated failure: that must not undo it.
    fn current(&self, ticket: u64) -> bool {
        self.ticket == ticket && self.since.is_some()
    }

    fn clear(&mut self, now: Instant) -> Clear {
        let Some(since) = self.since else { return Clear::Nothing };
        let left = FAILED_HOLD.saturating_sub(now.saturating_duration_since(since));
        if !left.is_zero() {
            return Clear::After(left, self.ticket);
        }
        self.since = None;
        Clear::Now
    }

    /// A `Clear::After` came due; true if the class goes.
    fn expire(&mut self, ticket: u64) -> bool {
        if self.ticket != ticket {
            return false;
        }
        self.since = None;
        true
    }
}

fn toggle_class(widget: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// `.hg-starting` survives the Busy(false) that ends a --demo start, so a
/// theme can preview it until the next attempt.
fn state_changes(event: &UiEvent) -> Vec<(&'static str, bool)> {
    match event {
        UiEvent::Busy(true) => {
            vec![("hg-busy", true), ("hg-failed", false), ("hg-starting", false)]
        }
        UiEvent::Busy(false) => vec![("hg-busy", false)],
        UiEvent::AuthError(_) | UiEvent::PamError(_) => {
            vec![("hg-failed", true), ("hg-info", false), ("hg-starting", false)]
        }
        UiEvent::Prompt { .. } => {
            vec![("hg-failed", false), ("hg-info", false), ("hg-starting", false)]
        }
        UiEvent::Info(text) => vec![("hg-failed", false), ("hg-info", !text.is_empty())],
        UiEvent::Armed(true) => {
            vec![("hg-armed", true), ("hg-failed", false), ("hg-starting", false)]
        }
        UiEvent::Armed(false) => vec![("hg-armed", false)],
        UiEvent::Starting => vec![("hg-starting", true)],
        UiEvent::SessionChanged(_) | UiEvent::Focus(_) | UiEvent::UsernameChanged(_) => Vec::new(),
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
    if let Some(dark) = dark_scheme(cfg) {
        set_color_scheme(&settings, dark);
    }
    match theme_change(cfg) {
        Theme::Set(theme) => settings.set_gtk_theme_name(Some(theme)),
        Theme::Reload => settings.set_gtk_theme_name(settings.gtk_theme_name().as_deref()),
        Theme::Keep => {}
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

/// Decided before the theme name is set: GTK has no Empty-dark, and Empty
/// loaded while the scheme is dark falls back to Default; turning the
/// scheme light afterwards only turns that Default light (GTK 4.24).
fn dark_scheme(cfg: &config::Gtk) -> Option<bool> {
    if cfg.theme.as_deref() == Some("Empty") {
        return Some(false);
    }
    cfg.dark
}

#[derive(Debug, PartialEq, Eq)]
enum Theme<'a> {
    Keep,
    Set(&'a str),
    /// The theme in force, set again.
    Reload,
}

/// With `dark` alone the theme in force is set again: from GTK 4.23 a
/// scheme change only re-renders the theme already loaded, and a theme
/// with a gtk-dark.css of its own (no media queries) keeps the variant it
/// was loaded with until a theme name is set.
fn theme_change(cfg: &config::Gtk) -> Theme<'_> {
    match (&cfg.theme, cfg.dark) {
        (Some(theme), _) => Theme::Set(theme),
        (None, Some(_)) => Theme::Reload,
        (None, None) => Theme::Keep,
    }
}

/// gtk-interface-color-scheme (GTK 4.20), what media queries see, is set
/// by name: the binding is built for 4.12. The theme variant follows it
/// from GTK 4.23, where prefer-dark became a deprecated alias that warns
/// on every write; before, the variant came from prefer-dark alone, so an
/// older GTK gets that as well.
fn set_color_scheme(settings: &gtk::Settings, dark: bool) {
    let scheme = settings.find_property(COLOR_SCHEME).and_then(|pspec| scheme_value(&pspec, dark));
    if let Some(scheme) = &scheme {
        settings.set_property_from_value(COLOR_SCHEME, scheme);
    }
    if prefer_dark_too(scheme.is_some(), (gtk::major_version(), gtk::minor_version())) {
        settings.set_gtk_application_prefer_dark_theme(dark);
    }
}

fn prefer_dark_too(has_scheme: bool, gtk: (u32, u32)) -> bool {
    !has_scheme || gtk < (4, 23)
}

/// By nick, from the enum behind `pspec`: the binding has no
/// GtkInterfaceColorScheme either.
fn scheme_value(pspec: &glib::ParamSpec, dark: bool) -> Option<glib::Value> {
    let class = glib::EnumClass::with_type(pspec.value_type())?;
    class.to_value_by_nick(if dark { "dark" } else { "light" })
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
    fn css_warnings_about_what_still_applies_stay_off_the_banner() {
        use gtk::CssParserWarning::{Deprecated, Syntax, Unimplemented};
        let warning = |kind, text| on_banner(&glib::Error::new(kind, text));
        assert!(!warning(Deprecated, "alpha() is deprecated"));
        assert!(!warning(Syntax, "Expected ';' at end of block"));
        assert!(!warning(Syntax, "Empty declaration"));
        assert!(warning(Syntax, "Undefined @media feature 'min-width'"));
        assert!(warning(Syntax, "Unterminated block at end of document"));
        assert!(warning(Unimplemented, "Undefined @media feature 'min-width'"));
        assert!(warning(Unimplemented, "Empty declaration"));
        for error in [gtk::CssParserError::Syntax, gtk::CssParserError::UnknownValue] {
            assert!(on_banner(&glib::Error::new(error, "No property named \"colour\"")));
            assert!(on_banner(&glib::Error::new(error, "Empty declaration")));
        }
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
    fn the_check_lists_and_counts_every_css_problem() {
        let lines: Vec<String> = (1..=7).map(|n| format!("style.css:{n}:1: bad")).collect();
        assert_eq!(css_lines(lines.clone(), false), capped(lines.clone(), CSS_PROBLEM_LINES));
        assert_eq!(css_lines(lines.clone(), true), lines);
        let (report, code) = crate::cli::check_report(&css_lines(lines, true));
        assert_eq!(report.len(), 8, "{report:?}");
        assert_eq!(report[6], "check: style.css:7:1: bad");
        assert_eq!((report[7].as_str(), code), ("check: 7 problems", 1));
    }

    fn gtk(dark: Option<bool>, theme: Option<&str>) -> config::Gtk {
        config::Gtk { dark, theme: theme.map(Into::into), ..Default::default() }
    }

    #[test]
    fn the_empty_theme_is_always_light() {
        assert_eq!(dark_scheme(&gtk(Some(true), Some("Empty"))), Some(false));
        assert_eq!(dark_scheme(&gtk(None, Some("Empty"))), Some(false));
        assert_eq!(dark_scheme(&gtk(Some(true), Some("adw-gtk3-dark"))), Some(true));
        assert_eq!(dark_scheme(&gtk(Some(false), Some("Default"))), Some(false));
        assert_eq!(dark_scheme(&gtk(Some(true), None)), Some(true));
        assert_eq!(dark_scheme(&gtk(None, Some("adw-gtk3"))), None);
    }

    #[test]
    fn the_colour_scheme_is_found_by_name_and_nick() {
        let settings = glib::Class::<gtk::Settings>::from_type(gtk::Settings::static_type());
        let Some(pspec) = settings.and_then(|class| class.find_property(COLOR_SCHEME)) else {
            assert!(gtk::check_version(4, 20, 0).is_some(), "GTK 4.20 has {COLOR_SCHEME}");
            return;
        };
        for (dark, nick) in [(true, "dark"), (false, "light")] {
            let value = scheme_value(&pspec, dark).expect(nick);
            assert_eq!(
                glib::EnumValue::from_value(&value).map(|(_, found)| found.nick()),
                Some(nick)
            );
        }
    }

    #[test]
    fn each_media_feature_takes_its_setting_s_type() {
        let settings = glib::Class::<gtk::Settings>::from_type(gtk::Settings::static_type());
        let provider = glib::Class::<gtk::CssProvider>::from_type(gtk::CssProvider::static_type());
        let (settings, provider) = (settings.unwrap(), provider.unwrap());
        let mut bound = 0;
        for (setting, feature) in MEDIA_FEATURES {
            let Some(pspec) = settings.find_property(setting) else { continue };
            let found = provider.find_property(feature).unwrap_or_else(|| panic!("{feature}"));
            assert_eq!(found.value_type(), pspec.value_type(), "a binding needs one type");
            bound += 1;
        }
        let expected = match (gtk::check_version(4, 20, 0), gtk::check_version(4, 22, 0)) {
            (Some(_), _) => 0,
            (None, Some(_)) => 2,
            (None, None) => 3,
        };
        assert_eq!(bound, expected);
    }

    #[test]
    fn prefer_dark_is_written_until_gtk_4_23() {
        assert!(!prefer_dark_too(true, (4, 24)));
        assert!(!prefer_dark_too(true, (4, 23)));
        assert!(prefer_dark_too(true, (4, 22)));
        assert!(prefer_dark_too(true, (4, 20)));
        assert!(prefer_dark_too(false, (4, 18)));
        assert!(prefer_dark_too(false, (4, 24)), "the scheme could not be written");
        assert!(!prefer_dark_too(true, (5, 0)));
    }

    #[test]
    fn under_empty_the_stylesheet_still_sees_dark() {
        assert_eq!(sheet_scheme(&gtk(Some(true), Some("Empty"))), Some(true));
        assert_eq!(sheet_scheme(&gtk(Some(false), Some("Empty"))), Some(false));
        assert_eq!(sheet_scheme(&gtk(None, Some("Empty"))), Some(false));
        assert_eq!(sheet_scheme(&gtk(Some(true), Some("adw-gtk3-dark"))), None);
        assert_eq!(sheet_scheme(&gtk(Some(true), None)), None);
        assert_eq!(sheet_scheme(&gtk(None, None)), None);
    }

    #[test]
    fn dark_alone_sets_the_theme_in_force_again() {
        assert_eq!(theme_change(&gtk(Some(true), None)), Theme::Reload);
        assert_eq!(theme_change(&gtk(Some(false), None)), Theme::Reload);
        assert_eq!(theme_change(&gtk(None, None)), Theme::Keep);
        assert_eq!(theme_change(&gtk(Some(true), Some("Split"))), Theme::Set("Split"));
        assert_eq!(theme_change(&gtk(None, Some("Empty"))), Theme::Set("Empty"));
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn a_failure_is_held_then_goes_with_the_next_clear() {
        let (mut failed, t0) = (FailedState::default(), Instant::now());
        let ticket = failed.fail(t0);
        assert!(failed.current(ticket));
        assert_eq!(failed.clear(t0 + ms(500)), Clear::After(ms(1000), ticket));
        assert!(failed.current(ticket), "still held");
        assert!(failed.expire(ticket));
        assert!(!failed.current(ticket));
        assert_eq!(failed.clear(t0 + ms(3000)), Clear::Nothing);
    }

    #[test]
    fn a_failure_right_after_a_clear_comes_back() {
        let (mut failed, t0) = (FailedState::default(), Instant::now());
        let first = failed.fail(t0);
        assert_eq!(failed.clear(t0 + FAILED_HOLD), Clear::Now);
        assert!(!failed.current(first));
        let second = failed.fail(t0 + FAILED_HOLD);
        assert!(failed.current(second));
        assert_eq!(failed.clear(t0 + FAILED_HOLD + ms(100)), Clear::After(ms(1400), second));
    }

    #[test]
    fn a_re_add_held_up_by_missing_frames_does_not_undo_a_clear() {
        let (mut failed, t0) = (FailedState::default(), Instant::now());
        failed.fail(t0);
        let repeated = failed.fail(t0 + ms(300));
        let Clear::After(_, ticket) = failed.clear(t0 + ms(400)) else {
            panic!("a clear inside the hold waits")
        };
        assert!(failed.expire(ticket));
        assert!(!failed.current(repeated));
        assert_eq!(failed.clear(t0 + ms(5000)), Clear::Nothing);

        let mut failed = FailedState::default();
        failed.fail(t0);
        let repeated = failed.fail(t0 + ms(300));
        assert_eq!(failed.clear(t0 + ms(3000)), Clear::Now);
        assert!(!failed.current(repeated));
    }

    #[test]
    fn a_hold_timer_from_before_the_next_failure_is_stale() {
        let (mut failed, t0) = (FailedState::default(), Instant::now());
        let first = failed.fail(t0);
        assert_eq!(failed.clear(t0 + ms(200)), Clear::After(ms(1300), first));
        let second = failed.fail(t0 + ms(800));
        assert!(!failed.expire(first));
        assert!(failed.current(second));
        assert_eq!(failed.clear(t0 + ms(900)), Clear::After(ms(1400), second));
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
            UiEvent::Info(String::new()),
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
        assert_eq!(on(&UiEvent::Busy(false), "hg-starting"), None);
        assert_eq!(on(&UiEvent::Info("demo: session would start now".into()), "hg-starting"), None);
        for next in [
            UiEvent::AuthError("could not start".into()),
            UiEvent::PamError("no".into()),
            UiEvent::Busy(true),
            UiEvent::Armed(true),
            UiEvent::Prompt { secret: true, text: "Password:".into(), passive: true },
        ] {
            assert_eq!(on(&next, "hg-starting"), Some(false), "{next:?}");
        }
        assert!(state_changes(&UiEvent::Focus(FocusTarget::Password)).is_empty());
        assert!(state_changes(&UiEvent::SessionChanged(1)).is_empty());
        assert!(state_changes(&UiEvent::UsernameChanged("bob".into())).is_empty());
    }

    fn classes_after(events: &[UiEvent]) -> Vec<&'static str> {
        let mut classes = std::collections::BTreeSet::new();
        for (class, on) in events.iter().flat_map(state_changes) {
            if on {
                classes.insert(class);
            } else {
                classes.remove(class);
            }
        }
        classes.into_iter().collect()
    }

    #[test]
    fn a_demo_start_stays_on_screen_until_the_next_attempt() {
        let started = vec![
            UiEvent::Busy(true),
            UiEvent::Busy(true),
            UiEvent::Starting,
            UiEvent::Info("demo: session would start now".into()),
            UiEvent::Busy(false),
        ];
        assert_eq!(classes_after(&started), ["hg-info", "hg-starting"]);
        let next = [started.clone(), vec![UiEvent::Busy(true)]].concat();
        assert_eq!(classes_after(&next), ["hg-busy", "hg-info"]);

        let refused =
            [&started[..3], &[UiEvent::Busy(false), UiEvent::AuthError("could not start".into())]]
                .concat();
        assert_eq!(classes_after(&refused), ["hg-failed"]);
    }
}
