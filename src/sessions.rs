use std::path::Path;

use crate::config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    for dir in data_dirs.split(':').filter(|d| !d.is_empty()) {
        for (sub, kind) in [("wayland-sessions", Kind::Wayland), ("xsessions", Kind::X11)] {
            let Ok(entries) = std::fs::read_dir(Path::new(dir).join(sub)) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "desktop") {
                    continue;
                }
                let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                if sessions.iter().any(|s| s.stem == stem && s.kind == kind) {
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
