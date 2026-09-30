//! Paths handed out by Flatpak's document portal.
//!
//! The app can read the home folder, but not write to it, so the file chooser returns
//! folders as `/run/user/<uid>/doc/<id>/<name>` aliases rather than their real location.
//! Each pick gets its own alias, so aliases can't be compared with each other (a folder
//! excluded inside a library folder wouldn't match it), and reading through them is
//! slower. Where the real path is readable, it is used instead.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use gtk::prelude::ToVariant;
use gtk::{gio, glib};

/// The real location of `path` if it is a document portal alias the app can read directly;
/// otherwise `path` itself.
pub fn host_path(path: &Path) -> PathBuf {
    let path = resolve(path).filter(|p| p.exists()).unwrap_or_else(|| path.to_owned());
    // Rebuilding from components drops trailing slashes, so paths display consistently.
    path.components().collect()
}

fn resolve(path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(glib::user_runtime_dir().join("doc")).ok()?;
    let mut parts = rel.components();
    let Some(Component::Normal(id)) = parts.next() else { return None };
    // The alias is `<id>/<name of the shared file>/<anything below it>`.
    parts.next()?;
    let id = id.to_str()?.to_owned();

    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
    let reply = bus
        .call_sync(
            Some("org.freedesktop.portal.Documents"),
            "/org/freedesktop/portal/documents",
            "org.freedesktop.portal.Documents",
            "GetHostPaths",
            Some(&(vec![id.clone()],).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        )
        .ok()?;
    let (paths,) = reply.get::<(HashMap<String, Vec<u8>>,)>()?;
    let bytes = paths.get(&id)?;
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let host = Path::new(std::ffi::OsStr::from_bytes(bytes));
    Some(host.join(parts.as_path()))
}
