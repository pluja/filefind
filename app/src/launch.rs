//! Opening files, through the desktop portal inside Flatpak.

use gtk::{gio, glib};

fn report(result: Result<(), glib::Error>, what: &str, on_error: impl FnOnce()) {
    if let Err(e) = result {
        if !e.matches(gtk::DialogError::Dismissed) {
            log::warn!("{what}: {e}");
            on_error();
        }
    }
}

pub fn open(path: &str, parent: Option<&gtk::Window>, on_error: impl FnOnce() + 'static) {
    gtk::FileLauncher::new(Some(&gio::File::for_path(path)))
        .launch(parent, gio::Cancellable::NONE, move |res| report(res, "open", on_error));
}

pub fn show_in_folder(path: &str, parent: Option<&gtk::Window>, on_error: impl FnOnce() + 'static) {
    gtk::FileLauncher::new(Some(&gio::File::for_path(path)))
        .open_containing_folder(parent, gio::Cancellable::NONE, move |res| report(res, "show in folder", on_error));
}
