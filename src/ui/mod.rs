//! Window shell, CSS loading, GTK settings. CSS problems are never fatal:
//! GTK skips bad rules, we log them; a missing file falls back to the
//! embedded default so the greeter always renders something styled.

use gtk4 as gtk;
use gtk4::prelude::*;
use std::path::Path;

use crate::config;
use crate::log::{error, info};

pub const DEFAULT_STYLE: &str = include_str!("../../data/style.css");

/// Load the user stylesheet (or the embedded default) at USER priority so it
/// wins over theme/application styles. Returns problems for the banner.
pub fn load_css(path: &Path) -> Vec<String> {
    let provider = gtk::CssProvider::new();
    provider.connect_parsing_error(|_, section, err| {
        // Non-fatal by design: GTK drops the bad rule and continues.
        error!("css {section}: {err}");
    });

    let mut problems = Vec::new();
    if path.exists() {
        info!("style: {}", path.display());
        provider.load_from_path(path);
    } else {
        // regreet silently skipped a bad --style path; be loud instead.
        problems.push(format!("style {} not found — using built-in style", path.display()));
        provider.load_from_string(DEFAULT_STYLE);
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
