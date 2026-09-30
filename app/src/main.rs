mod backend;
mod filters;
mod help;
mod i18n;
mod launch;
mod preferences;
mod portal;
mod preview;
mod providers;
mod render;
mod resize;
mod results;
mod settings;
mod sidebar;
mod window;

use std::cell::OnceCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::backend::Backend;

pub const APP_ID: &str = "io.github.pluja.Filefind";
pub const APP_NAME: &str = "Filefind";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Started at login to index without opening a window.
const BACKGROUND_ARG: &str = "--background";
/// Started over D-Bus (e.g. by a search provider) without opening a window.
const SERVICE_ARG: &str = "--gapplication-service";
/// How long the app lingers without windows after answering system searches.
const IDLE_EXIT_MS: u32 = 60_000;

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(filefind_core::helper::SERVER_ARG) {
        return glib::ExitCode::from(filefind_core::helper::serve() as u8);
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    glib::set_application_name(APP_NAME);

    // Development builds get their own ID so they can run next to the installed app.
    let app_id = if cfg!(debug_assertions) { format!("{APP_ID}.Devel") } else { APP_ID.to_owned() };
    let app = adw::Application::builder().application_id(app_id).build();
    // Development screenshots run next to an open instance instead of activating it.
    if std::env::var_os("FILEFIND_SNAPSHOT").is_some() {
        app.set_flags(gio::ApplicationFlags::NON_UNIQUE);
    }
    if args.iter().any(|a| a == SERVICE_ARG) {
        app.set_inactivity_timeout(IDLE_EXIT_MS);
    }

    let backend: Rc<OnceCell<Rc<Backend>>> = Rc::default();
    let background = args.iter().any(|a| a == BACKGROUND_ARG);
    app.connect_startup(glib::clone!(#[strong] backend, move |app| {
        load_style();
        gtk::Window::set_default_icon_name(APP_ID);
        for (action, accels) in [
            ("app.quit", &["<Control>q"][..]),
            ("window.close", &["<Control>w"]),
            ("win.toggle-sidebar", &["F9"]),
            ("win.add-folder", &["<Control>o"]),
            ("win.settings", &["<Control>comma"]),
            ("win.focus-search", &["<Control>f", "<Control>l"]),
        ] {
            app.set_accels_for_action(action, accels);
        }
        let quit = gio::ActionEntry::builder("quit").activate(|app: &adw::Application, _, _| app.quit()).build();
        app.add_action_entries([quit]);
        let b = Backend::new(app);
        i18n::init(b.settings().language.as_deref());
        providers::register(app, &b);
        let _ = backend.set(b);
    }));
    let first = std::cell::Cell::new(true);
    app.connect_activate(move |app| {
        let Some(backend) = backend.get() else { return };
        if first.replace(false) && background {
            // A login autostart that outlived the setting just exits.
            if backend.settings().background {
                backend.start_indexing();
            }
            return;
        }
        window::present(app, backend, None);
    });

    // GApplication only needs its own options; everything else is handled above.
    let gapp_args: Vec<&str> = args.iter().take(1).chain(args.iter().filter(|a| *a == SERVICE_ARG)).map(String::as_str).collect();
    app.run_with_args(&gapp_args)
}

fn load_style() {
    let Some(display) = gdk::Display::default() else { return };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    // The app's own symbolic icons, when running from the source tree.
    if cfg!(debug_assertions) {
        gtk::IconTheme::for_display(&display).add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../data/icons"));
    }
}
