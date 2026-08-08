//! hypred-greeter — a greetd greeter where the layout is yours.
//!
//! Exit codes (greeter-run.sh relies on this process always exiting):
//!   0   session started (greetd takes over)
//!   1   startup failure (bad CLI, no GREETD_SOCK outside --demo, GTK init)
//!   2   greetd IPC transport failure
//!   101 panic

mod cli;
#[macro_use]
mod log;

use gtk4 as gtk;
use gtk4::prelude::*;

fn main() {
    // A panic that unwinds into the GTK main loop wedges the greeter without
    // exiting — greetd then never starts a session (dark screen). Die loudly.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[hypred-greeter] panic: {info}");
        std::process::exit(101);
    }));

    let args = cli::parse();

    if !args.demo && std::env::var_os("GREETD_SOCK").is_none() {
        log::error!("GREETD_SOCK is not set and --demo not given; run under greetd or pass --demo");
        std::process::exit(1);
    }

    let app = gtk::Application::builder()
        .application_id("dev.edraven.hypred-greeter")
        .build();

    let demo = args.demo;
    app.connect_activate(move |app| {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("hypred-greeter")
            .build();
        if demo {
            window.set_default_size(1280, 800);
        } else {
            window.fullscreen();
        }
        window.present();
    });

    // gtk::Application swallows unknown argv; we already parsed ours.
    let exit = app.run_with_args::<&str>(&[]);
    std::process::exit(exit.get() as i32);
}
