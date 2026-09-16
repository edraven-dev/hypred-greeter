//! Exit codes: 0 session started, 1 startup failure, 2 greetd transport
//! failure, 101 panic. greetd starts the chosen session only after this
//! process exits, so it must always exit.

mod auth;
mod backend;
mod cli;
#[macro_use]
mod log;
mod config;
mod layout;
mod sessions;
mod state;
mod ui;
mod widgets;

use gtk4 as gtk;
use gtk4::prelude::*;
use std::rc::Rc;

fn main() {
    // A panic unwinding into the GTK main loop wedges the process without
    // exiting — that's a dark screen at boot.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[hypred-greeter] panic: {info}");
        std::process::exit(101);
    }));

    let args = cli::parse();

    if !args.demo && std::env::var_os("GREETD_SOCK").is_none() {
        log::error!("GREETD_SOCK is not set and --demo not given; run under greetd or pass --demo");
        std::process::exit(1);
    }

    let loaded = config::load(args.config.as_deref());
    for problem in &loaded.problems {
        log::error!("{problem}");
    }

    // NON_UNIQUE: GApplication would otherwise deduplicate a second demo
    // instance over DBus into the first one.
    let app = gtk::Application::builder()
        .application_id("dev.edraven.hypred-greeter")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let args = Rc::new(args);
    let loaded = Rc::new(loaded);
    app.connect_activate(move |app| {
        ui::apply_gtk_settings(&loaded.config.gtk);
        let style = loaded.resolve(args.style.as_deref(), &loaded.config.paths.style);
        let mut problems = loaded.problems.clone();
        problems.extend(ui::load_css(&style));

        let layout_path = loaded.resolve(args.layout.as_deref(), &loaded.config.paths.layout);
        let layout = layout::load(&layout_path, !args.demo);
        for problem in &layout.problems {
            log::error!("{problem}");
        }
        problems.extend(layout.problems);

        let backend: Box<dyn backend::Backend> = if args.demo {
            Box::new(backend::DemoBackend::new())
        } else {
            match backend::GreetdBackend::connect() {
                Ok(backend) => Box::new(backend),
                Err(err) => {
                    log::error!("{err}");
                    std::process::exit(1);
                }
            }
        };

        let session_list = sessions::discover();
        if session_list.is_empty() {
            problems.push("no sessions found in wayland-sessions/xsessions".into());
        }
        let saved = state::load();
        let initial_username = saved
            .last_user
            .clone()
            .or_else(|| if args.demo { std::env::var("USER").ok() } else { None })
            .unwrap_or_default();
        let shared = ui::ctx::Shared::new(initial_username, session_list, saved);

        let bus = Rc::new(ui::bus::Bus::default());
        let gtk_app = app.downgrade();
        let current = shared.clone();
        let resolver = shared.clone();
        let resolver_config = loaded.clone();
        let demo = args.demo;
        let auth = auth::Auth::start(
            backend,
            bus.clone(),
            args.demo,
            &loaded.config.auth,
            Box::new(move || current.username.borrow().clone()),
            Box::new(move |username: &str| {
                {
                    let mut state = resolver.state.borrow_mut();
                    state.last_user = Some(username.to_string());
                    if let Some(session) = resolver.selected() {
                        state.last_session.insert(username.to_string(), session.cache_id());
                    }
                    if !demo {
                        state::save(&state);
                    }
                }
                match resolver.selected() {
                    Some(session) => {
                        sessions::start_command(session, &resolver_config.config.sessions)
                    }
                    None => (Vec::new(), Vec::new()),
                }
            }),
            Box::new(move || {
                log::info!("session handed to greetd; exiting");
                if let Some(app) = gtk_app.upgrade() {
                    app.quit();
                } else {
                    std::process::exit(0);
                }
            }),
        );

        let handle = ui::ctx::AppHandle::new(auth, bus, shared);

        let registry = widgets::Registry::builtin();
        let ctx = ui::ctx::BuildCtx::new(
            &loaded.config,
            &loaded.base_dir,
            &registry,
            handle.clone(),
            args.demo,
        );
        let root = layout::build::build_node(&ctx, &layout.root);
        problems.extend(ctx.take_problems());

        let window =
            gtk::ApplicationWindow::builder().application(app).title("hypred-greeter").build();
        window.add_css_class("hg-window");
        window.set_widget_name("hg-window");
        window.set_child(Some(&ui::window_content(&problems, root)));
        if args.demo {
            window.set_default_size(1280, 800);
        } else {
            window.fullscreen();
        }
        window.present();

        // Capture phase: sees every key/pointer event before a child can
        // stop it.
        let activity = gtk::EventControllerLegacy::new();
        activity.set_propagation_phase(gtk::PropagationPhase::Capture);
        let watcher = handle.clone();
        activity.connect_event(move |_, event| {
            use gtk::gdk::EventType::{ButtonPress, KeyPress, MotionNotify, TouchBegin};
            match event.event_type() {
                // Deferred until the focused entry has handled the key, so
                // the first keystroke into the password entry already
                // counts as typing.
                KeyPress => {
                    let watcher = watcher.clone();
                    gtk::glib::idle_add_local_once(move || watcher.note_activity());
                }
                MotionNotify | ButtonPress | TouchBegin => watcher.note_activity(),
                _ => {}
            }
            gtk::glib::Propagation::Proceed
        });
        window.add_controller(activity);
        handle.auth.start_eager(&handle.username());
    });

    // Empty argv: GApplication would otherwise choke on our own flags.
    let exit = app.run_with_args::<&str>(&[]);
    std::process::exit(exit.get() as i32);
}
