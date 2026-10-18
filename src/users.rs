use std::collections::HashSet;
use std::path::Path;

use crate::config;

/// useradd's own when login.defs sets no UID_MIN/UID_MAX.
const UIDS: (u32, u32) = (1000, 60000);

pub fn initial(remembered: Option<&str>, auth: &config::Auth, demo_user: Option<&str>) -> String {
    let pinned = auth.user.as_deref().filter(|_| auth.fixed_user);
    pinned.or(remembered).or(auth.user.as_deref()).or(demo_user).unwrap_or_default().to_string()
}

pub fn discover() -> Vec<String> {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    local_users(&read("/etc/passwd"), &read("/etc/login.defs"), &read("/etc/shells"))
}

fn local_users(passwd: &str, login_defs: &str, shells: &str) -> Vec<String> {
    let (min, max) = uid_range(login_defs);
    let shells: HashSet<&str> = lines(shells).collect();
    passwd
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let [name, _, uid, _, _, _, shell] = fields[..] else { return None };
            let uid: u32 = uid.parse().ok()?;
            // passwd(5): an empty shell is /bin/sh.
            let shell = if shell.is_empty() { "/bin/sh" } else { shell };
            let no_login = matches!(
                Path::new(shell).file_name().and_then(|name| name.to_str()),
                Some("nologin" | "false")
            );
            let person = (min..=max).contains(&uid) && shells.contains(shell) && !no_login;
            (person && !name.is_empty() && !name.starts_with(['+', '-'])).then(|| name.to_string())
        })
        .collect()
}

fn uid_range(login_defs: &str) -> (u32, u32) {
    let mut range = UIDS;
    for line in lines(login_defs) {
        let mut words = line.split_whitespace();
        let slot = match words.next() {
            Some("UID_MIN") => &mut range.0,
            Some("UID_MAX") => &mut range.1,
            _ => continue,
        };
        if let Some(value) = words.next().and_then(|value| value.parse().ok()) {
            *slot = value;
        }
    }
    range
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// The user shown now is offered too, so a remembered or configured name
/// stays selectable.
pub fn pickable(users: Vec<String>, current: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in users.into_iter().chain((!current.is_empty()).then(|| current.to_string())) {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(user: Option<&str>, fixed_user: bool) -> config::Auth {
        config::Auth { user: user.map(str::to_string), fixed_user, ..Default::default() }
    }

    #[test]
    fn the_remembered_user_wins_unless_the_configured_one_is_pinned() {
        let configured = auth(Some("edraven"), false);
        assert_eq!(initial(Some("bob"), &configured, Some("me")), "bob");
        assert_eq!(initial(None, &configured, Some("me")), "edraven");
        assert_eq!(initial(None, &auth(None, false), Some("me")), "me");
        assert_eq!(initial(None, &auth(None, false), None), "");
        let pinned = auth(Some("edraven"), true);
        assert_eq!(initial(Some("bob"), &pinned, Some("me")), "edraven");
        assert_eq!(initial(None, &pinned, None), "edraven");
    }

    const PASSWD: &str = "\
root:x:0:0::/root:/usr/bin/bash
greeter:x:964:964::/var/lib/greeter:/usr/bin/nologin
edraven:x:1000:1000:E Draven:/home/edraven:/usr/bin/zsh
nobody:x:65534:65534:Kernel Overflow User:/:/usr/bin/nologin
edraven-work:x:1001:1001::/home/edraven-work:/bin/bash
locked:x:1002:1002::/home/locked:/usr/bin/nologin
git:x:1003:1003::/srv/git:/usr/bin/git-shell
plain:x:1004:1004::/home/plain:
falsy:x:1005:1005::/home/falsy:/bin/false
odd:x:1006:1006::/home/odd:/opt/unlisted-shell
broken line without fields
+nis:x:1007:1007::/home/nis:/bin/bash
";

    const SHELLS: &str = "\
# Pathnames of valid login shells.
/bin/sh
/bin/bash
/usr/bin/bash
/usr/bin/zsh
/usr/bin/git-shell
/bin/false
";

    const LOGIN_DEFS: &str = "\
# Min/max values for automatic uid selection in useradd
UID_MIN          1000
UID_MAX         60000
#UID_MAX 1001
SYS_UID_MIN       500
";

    #[test]
    fn local_users_have_a_user_uid_and_a_login_shell() {
        let users = local_users(PASSWD, LOGIN_DEFS, SHELLS);
        assert_eq!(users, ["edraven", "edraven-work", "git", "plain"]);
        assert_eq!(local_users(PASSWD, "", SHELLS), users);
        assert_eq!(
            local_users(PASSWD, "UID_MIN 1001\nUID_MAX\t1003\n", SHELLS),
            ["edraven-work", "git"]
        );
        assert!(local_users(PASSWD, LOGIN_DEFS, "").is_empty());
        assert!(local_users("", LOGIN_DEFS, SHELLS).is_empty());
    }

    #[test]
    fn a_bad_uid_bound_keeps_the_default() {
        assert_eq!(uid_range(""), (1000, 60000));
        assert_eq!(uid_range("UID_MIN lots\nUID_MAX 70000\n"), (1000, 70000));
        assert_eq!(uid_range("UID_MIN 500\nUID_MIN 2000\n"), (2000, 60000));
    }

    #[test]
    fn a_picker_offers_each_user_once_and_the_one_shown() {
        let users = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(pickable(users(&["a", "b", "a", ""]), "b"), ["a", "b"]);
        assert_eq!(pickable(users(&["a", "b"]), "fprint"), ["a", "b", "fprint"]);
        assert_eq!(pickable(users(&["a"]), ""), ["a"]);
        assert!(pickable(Vec::new(), "").is_empty());
    }
}
