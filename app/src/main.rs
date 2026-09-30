mod i18n;
mod sidebar;
mod window;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

pub const APP_ID: &str = "io.github.filefind.Filefind";
pub const APP_NAME: &str = "Filefind";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> glib::ExitCode {
    // Helper mode: extract one file's text in an isolated process (see filefind_core::extract).
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.len() == 3 && args[1] == "--extract" {
        return glib::ExitCode::from(filefind_core::extract::helper_main(args[2].as_ref()) as u8);
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    glib::set_application_name(APP_NAME);

    // Development builds get their own ID so they can run next to the installed app.
    let app_id = if cfg!(debug_assertions) { format!("{APP_ID}.Devel") } else { APP_ID.to_owned() };
    let app = adw::Application::builder()
        .application_id(app_id)
        .flags(gio::ApplicationFlags::default())
        .build();
    app.connect_startup(|app| {
        load_css();
        gtk::Window::set_default_icon_name(APP_ID);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("window.close", &["<Control>w"]);
        app.set_accels_for_action("win.toggle-sidebar", &["F9"]);
        app.set_accels_for_action("win.add-folder", &["<Control>o"]);
        app.set_accels_for_action("win.focus-search", &["<Control>f", "<Control>l"]);
        let quit = gio::ActionEntry::builder("quit").activate(|app: &adw::Application, _, _| app.quit()).build();
        app.add_action_entries([quit]);
    });
    app.connect_activate(|app| match app.active_window() {
        Some(win) => win.present(),
        None => window::build(app),
    });
    app.run_with_args::<&str>(&[])
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}
