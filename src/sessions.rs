//! Session discovery: .desktop entries from wayland-sessions/ and xsessions/
//! under every XDG_DATA_DIRS component. The parser is deliberately tiny —
//! we need five keys, not a freedesktop library.

use std::path::Path;

use crate::config;
use crate::log::warn_;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Wayland,
    X11,
}

#[derive(Debug, Clone)]
pub struct Session {
    /// Desktop entry Name — what the picker shows.
    pub name: String,
    pub exec: Vec<String>,
    pub kind: Kind,
    /// Filename stem; identity for the last-session cache.
    pub stem: String,
    /// DesktopNames (";"-separated) → XDG_CURRENT_DESKTOP.
    pub desktop_names: Option<String>,
}

pub fn discover() -> Vec<Session> {
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    let mut sessions: Vec<Session> = Vec::new();

    for dir in data_dirs.split(':').filter(|d| !d.is_empty()) {
        for (sub, kind) in [("wayland-sessions", Kind::Wayland), ("xsessions", Kind::X11)] {
            let Ok(entries) = std::fs::read_dir(Path::new(dir).join(sub)) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "desktop") {
                    continue;
                }
                let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                // Earlier XDG_DATA_DIRS entries win, same-stem duplicates skip.
                if sessions.iter().any(|s| s.stem == stem && s.kind == kind) {
                    continue;
                }
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        if let Some(session) = parse_desktop(&text, &stem, kind.clone()) {
                            sessions.push(session);
                        }
                    }
                    Err(err) => warn_!("session {}: {err}", path.display()),
                }
            }
        }
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}

fn parse_desktop(text: &str, stem: &str, kind: Kind) -> Option<Session> {
    let mut in_entry = false;
    let (mut name, mut exec, mut desktop_names) = (None, None, None);
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[') {
            in_entry = section.trim_end_matches(']') == "Desktop Entry";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            "Hidden" | "NoDisplay" if value.trim() == "true" => return None,
            "Name" if name.is_none() => name = Some(value.trim().to_string()),
            "Exec" if exec.is_none() => exec = shlex::split(value.trim()),
            "DesktopNames" => desktop_names = Some(value.trim().to_string()),
            _ => {}
        }
    }
    let exec = exec?;
    if exec.is_empty() {
        return None;
    }
    Some(Session {
        name: name.unwrap_or_else(|| stem.to_string()),
        exec,
        kind,
        stem: stem.to_string(),
        desktop_names,
    })
}

/// The start_session payload for a chosen session.
pub fn start_command(session: &Session, cfg: &config::Sessions) -> (Vec<String>, Vec<String>) {
    let mut cmd = Vec::new();
    if session.kind == Kind::X11 {
        cmd.extend(cfg.x11_prefix.iter().cloned());
    }
    cmd.extend(session.exec.iter().cloned());

    let session_type = match session.kind {
        Kind::Wayland => "wayland",
        Kind::X11 => "x11",
    };
    let mut env = vec![
        format!("XDG_SESSION_TYPE={session_type}"),
        format!("XDG_SESSION_DESKTOP={}", session.stem),
    ];
    if let Some(names) = &session.desktop_names {
        env.push(format!("XDG_CURRENT_DESKTOP={}", names.trim_end_matches(';').replace(';', ":")));
    }
    env.extend(cfg.env.iter().cloned());
    (cmd, env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_entry() {
        let text = "[Desktop Entry]\nName=Hyprland (uwsm-managed)\nExec=uwsm start -- hyprland.desktop\nDesktopNames=Hyprland;\n";
        let session = parse_desktop(text, "hyprland-uwsm", Kind::Wayland).unwrap();
        assert_eq!(session.name, "Hyprland (uwsm-managed)");
        assert_eq!(session.exec, ["uwsm", "start", "--", "hyprland.desktop"]);

        let cfg = config::Sessions::default();
        let (cmd, env) = start_command(&session, &cfg);
        assert_eq!(cmd, session.exec);
        assert!(env.contains(&"XDG_SESSION_TYPE=wayland".to_string()));
        assert!(env.contains(&"XDG_CURRENT_DESKTOP=Hyprland".to_string()));
    }

    #[test]
    fn hidden_entries_and_other_sections_are_skipped() {
        assert!(parse_desktop("[Desktop Entry]\nHidden=true\nExec=x\n", "x", Kind::X11).is_none());
        let text = "[Other]\nExec=wrong\n[Desktop Entry]\nExec=right\n";
        let session = parse_desktop(text, "s", Kind::X11).unwrap();
        assert_eq!(session.exec, ["right"]);
    }

    #[test]
    fn x11_gets_the_prefix() {
        let session = parse_desktop("[Desktop Entry]\nExec=plasma\n", "plasma", Kind::X11).unwrap();
        let cfg = config::Sessions::default();
        let (cmd, env) = start_command(&session, &cfg);
        assert_eq!(cmd, ["startx", "/usr/bin/env", "plasma"]);
        assert!(env.contains(&"XDG_SESSION_TYPE=x11".to_string()));
    }
}
