use std::collections::HashSet;
use std::path::Path;

use crate::config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Wayland,
    X11,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Wayland => "wayland",
            Kind::X11 => "x11",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "wayland" => Some(Kind::Wayland),
            "x11" => Some(Kind::X11),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub name: String,
    pub exec: Vec<String>,
    pub kind: Kind,
    pub stem: String,
    pub desktop_names: Option<String>,
}

impl Session {
    /// Kind-qualified: a Wayland and an X11 session may share a stem
    /// (GNOME ships gnome.desktop in both dirs).
    pub fn cache_id(&self) -> String {
        format!("{}/{}", self.kind.as_str(), self.stem)
    }

    pub fn matches_cache_id(&self, cached: &str) -> bool {
        match cached.split_once('/') {
            Some((kind, stem)) => Kind::parse(kind) == Some(self.kind) && stem == self.stem,
            None => cached == self.stem,
        }
    }
}

pub fn discover() -> Vec<Session> {
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    discover_in(&data_dirs)
}

fn discover_in(data_dirs: &str) -> Vec<Session> {
    let mut sessions: Vec<Session> = Vec::new();
    // Every stem met, hidden ones included: a NoDisplay copy in an earlier
    // dir masks the later ones, as XDG has it.
    let mut seen: HashSet<(Kind, String)> = HashSet::new();

    for dir in data_dirs.split(':').filter(|d| !d.is_empty()) {
        for (sub, kind) in [("wayland-sessions", Kind::Wayland), ("xsessions", Kind::X11)] {
            let Ok(entries) = std::fs::read_dir(Path::new(dir).join(sub)) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "desktop") {
                    continue;
                }
                let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                if !seen.insert((kind, stem.clone())) {
                    continue;
                }
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        if let Some(session) = parse_desktop(&text, &stem, kind) {
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

/// `[sessions]` only/hide, names and the X11 suffix, then the order —
/// once, so every consumer after it sees the same list. Ids that match
/// nothing are reported, not fatal.
pub fn apply(
    mut sessions: Vec<Session>,
    cfg: &config::Sessions,
    problems: &mut Vec<String>,
) -> Vec<Session> {
    let listed = [("only", &cfg.only), ("hide", &cfg.hide), ("order", &cfg.order)];
    let names: Vec<String> = cfg.names.keys().cloned().collect();
    for (key, ids) in listed.into_iter().chain([("names", &names)]) {
        for id in ids.iter().filter(|id| !sessions.iter().any(|s| s.matches_cache_id(id))) {
            problems.push(format!("sessions.{key}: `{id}` matches no installed session"));
        }
    }

    let installed = sessions.len();
    if !cfg.only.is_empty() {
        sessions.retain(|s| cfg.only.iter().any(|id| s.matches_cache_id(id)));
    }
    sessions.retain(|s| !cfg.hide.iter().any(|id| s.matches_cache_id(id)));
    if installed > 0 && sessions.is_empty() {
        problems.push("sessions: `only`/`hide` leave no session".into());
    }
    for session in &mut sessions {
        match cfg.names.iter().find(|(id, _)| session.matches_cache_id(id)) {
            Some((_, name)) => session.name = name.clone(),
            None if session.kind == Kind::X11 => session.name.push_str(&cfg.x11_suffix),
            None => {}
        }
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    let rank = |s: &Session| {
        cfg.order.iter().position(|id| s.matches_cache_id(id)).unwrap_or(cfg.order.len())
    };
    sessions.sort_by_key(rank);
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

pub fn start_command(session: &Session, cfg: &config::Sessions) -> (Vec<String>, Vec<String>) {
    let mut cmd = Vec::new();
    if session.kind == Kind::X11 {
        cmd.extend(cfg.x11_prefix.iter().cloned());
    }
    cmd.extend(session.exec.iter().cloned());

    let mut env = vec![
        format!("XDG_SESSION_TYPE={}", session.kind.as_str()),
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

    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("hg-sessions-{tag}-{}", std::process::id()));
            std::fs::remove_dir_all(&root).ok();
            Self(root)
        }

        fn write(&self, rel: &str, content: &str) -> &Self {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
            self
        }

        fn dirs(&self, subdirs: &[&str]) -> String {
            subdirs
                .iter()
                .map(|d| self.0.join(d).to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(":")
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn earlier_data_dir_wins_for_the_same_stem() {
        let tree = Tree::new("precedence");
        tree.write("a/wayland-sessions/hypr.desktop", "[Desktop Entry]\nName=First\nExec=one\n")
            .write("b/wayland-sessions/hypr.desktop", "[Desktop Entry]\nName=Second\nExec=two\n");
        let sessions = discover_in(&tree.dirs(&["a", "b"]));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "First");
        assert_eq!(sessions[0].exec, ["one"]);
    }

    #[test]
    fn a_hidden_copy_in_an_earlier_data_dir_masks_the_later_one() {
        let tree = Tree::new("masking");
        tree.write(
            "a/wayland-sessions/hypr.desktop",
            "[Desktop Entry]\nNoDisplay=true\nExec=one\n",
        )
        .write("b/wayland-sessions/hypr.desktop", "[Desktop Entry]\nName=Second\nExec=two\n")
        .write("b/wayland-sessions/sway.desktop", "[Desktop Entry]\nName=Sway\nExec=sway\n");
        let sessions = discover_in(&tree.dirs(&["a", "b"]));
        let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Sway"]);
    }

    fn found(entries: &[(&str, &str, Kind)]) -> Vec<Session> {
        entries
            .iter()
            .map(|(stem, name, kind)| Session {
                name: name.to_string(),
                exec: vec![stem.to_string()],
                kind: *kind,
                stem: stem.to_string(),
                desktop_names: None,
            })
            .collect()
    }

    fn installed() -> Vec<Session> {
        found(&[
            ("hyprland", "Hyprland", Kind::Wayland),
            ("hyprland-uwsm", "Hyprland (uwsm-managed)", Kind::Wayland),
            ("plasma", "Plasma", Kind::X11),
            ("sway", "Sway", Kind::Wayland),
        ])
    }

    fn names(sessions: &[Session]) -> Vec<&str> {
        sessions.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn apply_with_defaults_only_adds_the_x11_suffix() {
        let mut problems = Vec::new();
        let sessions = apply(installed(), &config::Sessions::default(), &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
        let expected = ["Hyprland", "Hyprland (uwsm-managed)", "Plasma (X11)", "Sway"];
        assert_eq!(names(&sessions), expected);
    }

    #[test]
    fn apply_only_and_hide_filter_by_id() {
        let mut problems = Vec::new();
        let cfg = config::Sessions {
            only: vec!["wayland/hyprland-uwsm".into(), "x11/plasma".into(), "wayland/sway".into()],
            hide: vec!["wayland/sway".into()],
            x11_suffix: String::new(),
            ..Default::default()
        };
        let sessions = apply(installed(), &cfg, &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(names(&sessions), ["Hyprland (uwsm-managed)", "Plasma"]);
    }

    #[test]
    fn apply_renames_then_orders_the_listed_first_and_the_rest_by_name() {
        let mut problems = Vec::new();
        let cfg = config::Sessions {
            order: vec!["wayland/sway".into(), "wayland/hyprland-uwsm".into()],
            names: [("x11/plasma".to_string(), "KDE".to_string())].into(),
            ..Default::default()
        };
        let sessions = apply(installed(), &cfg, &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(names(&sessions), ["Sway", "Hyprland (uwsm-managed)", "Hyprland", "KDE"]);
        assert_eq!(sessions[1].cache_id(), "wayland/hyprland-uwsm");
    }

    #[test]
    fn apply_reports_ids_that_match_nothing() {
        let mut problems = Vec::new();
        let cfg = config::Sessions {
            only: vec!["wayland/hyprland".into(), "wayland/gnome".into()],
            hide: vec!["x11/i3".into()],
            order: vec!["wayland/typo".into()],
            names: [("wayland/nope".to_string(), "Nope".to_string())].into(),
            ..Default::default()
        };
        let sessions = apply(installed(), &cfg, &mut problems);
        assert_eq!(names(&sessions), ["Hyprland"]);
        problems.sort();
        let expected = [
            "sessions.hide: `x11/i3` matches no installed session",
            "sessions.names: `wayland/nope` matches no installed session",
            "sessions.only: `wayland/gnome` matches no installed session",
            "sessions.order: `wayland/typo` matches no installed session",
        ];
        assert_eq!(problems, expected);
    }

    #[test]
    fn a_filter_that_leaves_nothing_is_reported() {
        let mut problems = Vec::new();
        let cfg = config::Sessions { only: vec!["wayland/nope".into()], ..Default::default() };
        assert!(apply(installed(), &cfg, &mut problems).is_empty());
        let expected = [
            "sessions.only: `wayland/nope` matches no installed session",
            "sessions: `only`/`hide` leave no session",
        ];
        assert_eq!(problems, expected);
    }

    #[test]
    fn same_stem_across_kinds_keeps_both() {
        let tree = Tree::new("kinds");
        tree.write("d/wayland-sessions/gnome.desktop", "[Desktop Entry]\nName=GNOME\nExec=gw\n")
            .write("d/xsessions/gnome.desktop", "[Desktop Entry]\nName=GNOME\nExec=gx\n");
        let sessions = discover_in(&tree.dirs(&["d"]));
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|s| s.kind == Kind::Wayland && s.exec == ["gw"]));
        assert!(sessions.iter().any(|s| s.kind == Kind::X11 && s.exec == ["gx"]));
    }

    #[test]
    fn non_desktop_files_skip_and_names_sort() {
        let tree = Tree::new("misc");
        tree.write("d/wayland-sessions/zeta.desktop", "[Desktop Entry]\nName=Zeta\nExec=z\n")
            .write("d/wayland-sessions/alpha.desktop", "[Desktop Entry]\nName=Alpha\nExec=a\n")
            .write("d/wayland-sessions/README", "not a session")
            .write("d/wayland-sessions/broken.desktop.bak", "[Desktop Entry]\nExec=nope\n");
        let sessions = discover_in(&format!(":{}:", tree.dirs(&["d"])));
        let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Zeta"]);
    }
}
