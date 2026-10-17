use crate::config;

pub fn initial(remembered: Option<&str>, auth: &config::Auth, demo_user: Option<&str>) -> String {
    let pinned = auth.user.as_deref().filter(|_| auth.fixed_user);
    pinned.or(remembered).or(auth.user.as_deref()).or(demo_user).unwrap_or_default().to_string()
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
}
