//! Exit codes: 0 session started, 1 startup failure, 2 greetd transport
//! failure, 101 panic; with --check 0 nothing to report, 1 problems or no
//! check at all.
//! greetd starts the chosen session only after this process exits, so it
//! must always exit.

mod actions;
mod auth;
mod backend;
mod cli;
#[macro_use]
mod log;
mod config;
mod layout;
mod seat;
mod sessions;
mod state;
mod ui;
mod users;
mod widgets;

use gtk4 as gtk;
use gtk4::prelude::*;
use std::rc::Rc;
use std::time::Duration;

use ui::bus::{FocusTarget, UiEvent};

fn main() {
    // A panic unwinding into the GTK main loop wedges the process without
    // exiting — that's a dark screen at boot.
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        std::process::exit(101);
    }));

    let args = cli::parse();
    log::init(args.demo || args.check);

    if args.greetd() && std::env::var_os("GREETD_SOCK").is_none() {
        log::error!("GREETD_SOCK is not set; run under greetd, or pass --demo or --check");
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
        let css = ui::load_css(&style, &loaded.config.gtk);
        problems.extend(ui::css_lines(css, args.check));

        let saved = if args.reads_state() { state::load() } else { state::State::default() };
        let demo_user = if args.demo { std::env::var("USER").ok() } else { None };
        let initial_username =
            users::initial(saved.last_user.as_deref(), &loaded.config.auth, demo_user.as_deref());

        let layout_path = loaded.resolve(args.layout.as_deref(), &loaded.config.paths.layout);
        let layout = layout::load(&layout_path, !args.demo, !initial_username.is_empty());
        for problem in &layout.problems {
            log::error!("{problem}");
        }
        problems.extend(layout.problems);

        let backend: Box<dyn backend::Backend> = if args.greetd() {
            match backend::GreetdBackend::connect() {
                Ok(backend) => Box::new(backend),
                Err(err) => {
                    log::error!("{err}");
                    std::process::exit(1);
                }
            }
        } else {
            Box::new(backend::DemoBackend::new())
        };

        let discovered = sessions::discover();
        let session_list =
            sessions::apply(discovered.clone(), &loaded.config.sessions, &mut problems);
        if discovered.is_empty() {
            problems.push("no sessions found in wayland-sessions/xsessions".into());
        }
        let default_session = loaded.config.sessions.default.as_deref();
        if let Some(id) = default_session {
            if !session_list.iter().any(|s| s.matches_cache_id(id)) {
                let why = if discovered.iter().any(|s| s.matches_cache_id(id)) {
                    "is excluded by [sessions] only/hide"
                } else {
                    "matches no installed session"
                };
                problems.push(format!("sessions.default `{id}` {why}"));
            }
        }
        let shared = ui::ctx::Shared::new(initial_username, session_list, saved, default_session);

        let bus = Rc::new(ui::bus::Bus::default());
        let gtk_app = app.downgrade();
        let current = shared.clone();
        let resolver = shared.clone();
        let resolver_config = loaded.clone();
        let (demo, persist) = (args.demo, args.greetd());
        let auth = auth::Auth::start(
            backend,
            bus.clone(),
            args.demo,
            &loaded.config.auth,
            &loaded.config.texts,
            auth::Hooks {
                current_username: Box::new(move || current.username.borrow().clone()),
                resolve_start: Box::new(move |username: &str| {
                    {
                        let mut state = resolver.state.borrow_mut();
                        state.last_user = Some(username.to_string());
                        if let Some(session) = resolver.selected() {
                            state.last_session.insert(username.to_string(), session.cache_id());
                        }
                        if persist {
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
                on_started: Box::new(move || {
                    log::info!("session handed to greetd; exiting");
                    if let Some(app) = gtk_app.upgrade() {
                        app.quit();
                    } else {
                        std::process::exit(0);
                    }
                }),
                seat_active: if demo { Box::new(|| true) } else { Box::new(seat::active) },
            },
        );

        let debounce = Duration::from_millis(loaded.config.auth.username_debounce_ms);
        let handle = ui::ctx::AppHandle::new(auth, bus, shared, debounce);

        let registry = widgets::Registry::builtin();
        let ctx = ui::ctx::BuildCtx::new(
            &loaded.config,
            &loaded.base_dir,
            &registry,
            handle.clone(),
            args.demo,
            args.check,
        );
        let root = layout::build::build_node(&ctx, &layout.root);
        problems.extend(ctx.take_problems());
        let focus_placed = layout::requests_focus(&layout.root);
        let shortcuts = actions::shortcuts(&loaded.config, &handle, args.demo, &mut problems);

        // Every problem source comes before this: --check ends here.
        if args.check {
            let (lines, code) = cli::check_report(&problems);
            for line in lines {
                eprintln!("[hypred-greeter] {line}");
            }
            std::process::exit(code);
        }

        let window =
            gtk::ApplicationWindow::builder().application(app).title("hypred-greeter").build();
        window.add_css_class("hg-window");
        window.set_widget_name("hg-window");
        if args.demo {
            window.add_css_class("hg-demo");
        }
        if !problems.is_empty() {
            window.add_css_class("hg-problems");
        }
        window.set_child(Some(&ui::window_content(&problems, root)));
        ui::track_states(&window, &handle);
        handle.emit(&UiEvent::UsernameChanged(handle.username()));
        if args.demo {
            // No title bar: the published viewport is the surface's size,
            // and the live greeter has none either.
            window.set_decorated(false);
            window.set_default_size(1280, 800);
        } else {
            window.fullscreen();
        }
        ui::publish_viewport(&window);
        // Before present: GTK would otherwise focus the first entry on show
        // (selecting its text) before the focus moves on.
        if !focus_placed {
            let target = if handle.username().is_empty() {
                FocusTarget::Username
            } else {
                FocusTarget::Password
            };
            handle.emit(&UiEvent::Focus(target));
        }
        window.present();

        // GTK runs the controller added last first, and a key a shortcut
        // takes goes no further: the activity watcher is added after these,
        // so a bound key still counts as input.
        for controller in shortcuts {
            window.add_controller(controller);
        }
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
