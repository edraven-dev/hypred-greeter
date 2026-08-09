//! Window shell, CSS loading, GTK settings. CSS problems are never fatal:
//! GTK skips bad rules, we log them; a missing or unreadable file falls
//! back to the embedded default so the greeter always renders something
//! styled.

pub mod bus;
pub mod ctx;

use gtk4 as gtk;
use gtk4::prelude::*;
use std::path::Path;

use crate::config;

pub const DEFAULT_STYLE: &str = include_str!("../../data/style.css");

/// Window content: the built layout tree, with a problem banner stacked on
/// top when config/layout/css loading recorded anything worth showing.
pub fn window_content(problems: &[String], root: gtk::Widget) -> gtk::Widget {
    root.set_hexpand(true);
    root.set_vexpand(true);
    if problems.is_empty() {
        return root;
    }
    let banner = gtk::Label::builder().label(problems.join("\n")).wrap(true).xalign(0.0).build();
    banner.add_css_class("hg-banner");
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&banner);
    column.append(&root);
    column.upcast()
}

/// Load the user stylesheet (or the embedded default) at USER priority so it
/// wins over theme/application styles. Returns problems for the banner.
pub fn load_css(path: &Path) -> Vec<String> {
    let provider = gtk::CssProvider::new();
    provider.connect_parsing_error(|_, section, err| {
        // Non-fatal by design: GTK drops the bad rule and continues.
        error!("css {section}: {err}");
    });

    let mut problems = Vec::new();
    // Probe with a read, not exists(): an unreadable file (bad permissions
    // after a root chown, broken ACL) must degrade exactly like a missing
    // one — defaults plus a banner. regreet silently skipped bad --style
    // paths; be loud instead. On success load_from_path so relative url()
    // references in the stylesheet still resolve against its directory.
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

    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("no display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );
    problems
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
