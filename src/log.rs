//! Leveled stderr logging. greetd forwards the greeter's stderr to the
//! journal, so plain eprintln with a level tag is all the machinery needed.

macro_rules! info {
    ($($arg:tt)*) => { eprintln!("[hypred-greeter] {}", format_args!($($arg)*)) };
}

macro_rules! warn_ {
    ($($arg:tt)*) => { eprintln!("[hypred-greeter] warning: {}", format_args!($($arg)*)) };
}

macro_rules! error {
    ($($arg:tt)*) => { eprintln!("[hypred-greeter] error: {}", format_args!($($arg)*)) };
}

pub(crate) use {error, info, warn_};
