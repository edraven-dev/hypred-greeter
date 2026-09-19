//! greetd runs one greeter per VT; several may be up at once (a second
//! instance for a second user). Only the one on screen may hold the
//! fingerprint reader, and each remembers its own last user.

/// The greeter's VT number, from the XDG_VTNR greetd exports.
pub fn vt() -> Option<String> {
    let vt = std::env::var("XDG_VTNR").ok()?;
    (!vt.is_empty() && vt.bytes().all(|b| b.is_ascii_digit())).then_some(vt)
}

/// Whether that VT is the one on screen; true when it cannot be told.
pub fn active() -> bool {
    let Some(vt) = vt() else { return true };
    match std::fs::read_to_string("/sys/class/tty/tty0/active") {
        Ok(on_screen) => is_on_screen(&vt, &on_screen),
        Err(_) => true,
    }
}

/// `on_screen`: the content of /sys/class/tty/tty0/active ("tty2\n").
fn is_on_screen(vt: &str, on_screen: &str) -> bool {
    on_screen.trim().strip_prefix("tty") == Some(vt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_active_vt_is_on_screen() {
        assert!(is_on_screen("2", "tty2\n"));
        assert!(!is_on_screen("2", "tty1\n"));
        assert!(!is_on_screen("1", "tty12\n"));
        assert!(!is_on_screen("2", ""));
    }
}
