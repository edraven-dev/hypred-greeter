//! Every line goes to stderr. A greeter's stderr is rarely the journal —
//! Hyprland's exec hands its children /dev/null, greetd a VT — so unless it
//! is, or under --demo or --check, each line also goes to journald's native
//! socket, and so do GLib's and GTK's messages: the log does not depend on
//! how the greeter was started.

use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixDatagram;
use std::sync::OnceLock;

const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";
const IDENTIFIER: &str = "hypred-greeter";

static JOURNAL: OnceLock<UnixDatagram> = OnceLock::new();

#[derive(Clone, Copy)]
pub enum Level {
    Error,
    Warning,
    Info,
}

impl Level {
    fn priority(self) -> u8 {
        match self {
            Level::Error => 3,
            Level::Warning => 4,
            Level::Info => 6,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Level::Error => "error: ",
            Level::Warning => "warning: ",
            Level::Info => "",
        }
    }
}

pub fn write(level: Level, message: std::fmt::Arguments) {
    let message = message.to_string();
    let line = format!("[{IDENTIFIER}] {}{message}\n", level.prefix());
    // Not eprintln: a stderr gone bad must not panic the greeter.
    std::io::stderr().write_all(line.as_bytes()).ok();
    if let Some(journal) = JOURNAL.get() {
        journal.send_to(&entry(level.priority(), &message), JOURNAL_SOCKET).ok();
    }
}

/// Before anything is logged; never in tests (GLib takes one writer only).
pub fn init(greetd: bool) {
    let stderr = std::io::stderr();
    let stderr_id = stderr
        .as_fd()
        .try_clone_to_owned()
        .ok()
        .and_then(|fd| std::fs::File::from(fd).metadata().ok())
        .map(|meta| (meta.dev(), meta.ino()));
    let stream = std::env::var("JOURNAL_STREAM").ok();
    let stderr_is_journal = glib::log_writer_is_journald(&stderr);
    if !needs_journal(greetd, stderr_is_journal, stream.as_deref(), stderr_id) {
        return;
    }
    if !std::fs::metadata(JOURNAL_SOCKET).is_ok_and(|meta| meta.file_type().is_socket()) {
        return;
    }
    let Ok(socket) = UnixDatagram::unbound() else { return };
    JOURNAL.set(socket).ok();
    glib::log_set_writer_func(glib_writer);
}

/// systemd-cat and systemd services connect stderr to the journal and say
/// so in JOURNAL_STREAM (`dev:inode` of that stream); a child that merely
/// inherited the variable has another stderr.
fn needs_journal(
    greetd: bool,
    stderr_is_journal: bool,
    journal_stream: Option<&str>,
    stderr: Option<(u64, u64)>,
) -> bool {
    if !greetd || stderr_is_journal {
        return false;
    }
    let stream = journal_stream.and_then(|value| {
        let (dev, ino) = value.split_once(':')?;
        Some((dev.parse().ok()?, ino.parse().ok()?))
    });
    stream.is_none() || stream != stderr
}

/// journald's native protocol: `KEY=value\n`, or, for a value holding a
/// newline, `KEY\n` + its length as a 64-bit little-endian + value + `\n`.
fn entry(priority: u8, message: &str) -> Vec<u8> {
    let mut entry = Vec::with_capacity(message.len() + 64);
    let priority = priority.to_string();
    let fields =
        [("PRIORITY", priority.as_str()), ("SYSLOG_IDENTIFIER", IDENTIFIER), ("MESSAGE", message)];
    for (key, value) in fields {
        entry.extend_from_slice(key.as_bytes());
        if value.contains('\n') {
            entry.push(b'\n');
            entry.extend_from_slice(&(value.len() as u64).to_le_bytes());
        } else {
            entry.push(b'=');
        }
        entry.extend_from_slice(value.as_bytes());
        entry.push(b'\n');
    }
    entry
}

/// GLib's journald writer, unlike its default one, keeps debug and info
/// messages; the default one still writes stderr.
fn glib_writer(level: glib::LogLevel, fields: &[glib::LogField<'_>]) -> glib::LogWriterOutput {
    let domain = fields.iter().find(|field| field.key() == "GLIB_DOMAIN");
    if !glib::log_writer_default_would_drop(level, domain.and_then(|field| field.value_str())) {
        glib::log_writer_journald(level, fields);
    }
    glib::log_writer_default(level, fields)
}

macro_rules! info {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Info, format_args!($($arg)*)) };
}

macro_rules! warn_ {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Warning, format_args!($($arg)*)) };
}

macro_rules! error {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Error, format_args!($($arg)*)) };
}

pub(crate) use {error, info, warn_};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_journal_is_written_only_where_stderr_is_not_it() {
        let stderr = Some((10, 10136));
        assert!(!needs_journal(false, false, None, None), "--demo or --check");
        assert!(!needs_journal(true, true, None, stderr), "stderr connected to the journal");
        assert!(!needs_journal(true, false, Some("10:10136"), stderr), "systemd-cat");
        assert!(needs_journal(true, false, Some("10:10136"), Some((5, 4))), "inherited");
        assert!(needs_journal(true, false, None, stderr), "no JOURNAL_STREAM");
        assert!(needs_journal(true, false, Some("10-10136"), stderr), "malformed");
        assert!(needs_journal(true, false, Some("10:10136"), None), "stderr closed");
    }

    #[test]
    fn an_entry_speaks_journalds_native_protocol() {
        let plain = b"PRIORITY=6\nSYSLOG_IDENTIFIER=hypred-greeter\nMESSAGE=style: a.css\n";
        assert_eq!(entry(6, "style: a.css"), plain);
        let mut lines = b"PRIORITY=3\nSYSLOG_IDENTIFIER=hypred-greeter\nMESSAGE\n".to_vec();
        lines.extend_from_slice(&12u64.to_le_bytes());
        lines.extend_from_slice(b"panic: a\nb=c\n");
        assert_eq!(entry(3, "panic: a\nb=c"), lines);
    }
}
