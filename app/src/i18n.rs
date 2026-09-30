//! Translations. Catalogs are standard gettext `.mo` files, looked up by the
//! user's language (LANGUAGE / LC_ALL / LC_MESSAGES / LANG). They are read
//! directly, so translations work even when the system lacks locale data.

use std::path::PathBuf;
use std::sync::OnceLock;

use gettext::Catalog;
use gtk::glib;

const DOMAIN: &str = "filefind";

struct Translations {
    catalog: Option<Catalog>,
    language: String,
}

static TRANSLATIONS: OnceLock<Translations> = OnceLock::new();

fn locale_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("FILEFIND_LOCALEDIR") {
        dirs.push(PathBuf::from(dir));
    }
    // Installed layout: <prefix>/bin/filefind and <prefix>/share/locale.
    if let Some(prefix) = std::env::current_exe().ok().and_then(|exe| exe.parent()?.parent().map(PathBuf::from)) {
        dirs.push(prefix.join("share/locale"));
    }
    dirs.push(PathBuf::from("/app/share/locale"));
    dirs
}

fn load() -> Translations {
    let dirs = locale_dirs();
    for name in glib::language_names() {
        let name = name.as_str();
        if name == "C" || name == "POSIX" {
            break;
        }
        for dir in &dirs {
            let file = dir.join(name).join("LC_MESSAGES").join(format!("{DOMAIN}.mo"));
            if let Ok(f) = std::fs::File::open(&file) {
                match Catalog::parse(f) {
                    Ok(catalog) => {
                        let language = name.split(['_', '.', '@']).next().unwrap_or(name).to_owned();
                        return Translations { catalog: Some(catalog), language };
                    }
                    Err(e) => log::warn!("{}: {e}", file.display()),
                }
            }
        }
    }
    Translations { catalog: None, language: "en".into() }
}

fn translations() -> &'static Translations {
    TRANSLATIONS.get_or_init(load)
}

/// Translates a message.
pub fn tr(msgid: &str) -> String {
    match &translations().catalog {
        Some(c) => c.gettext(msgid).to_owned(),
        None => msgid.to_owned(),
    }
}

/// Translates a message with a plural form chosen by `n`.
pub fn ntr(msgid: &str, plural: &str, n: u64) -> String {
    match &translations().catalog {
        Some(c) => c.ngettext(msgid, plural, n).to_owned(),
        None if n == 1 => msgid.to_owned(),
        None => plural.to_owned(),
    }
}

/// Language code of the active translation, e.g. "es"; "en" when untranslated.
pub fn language() -> &'static str {
    &translations().language
}

/// Formats a count with the thousands separator of the active language.
pub fn fmt_count(n: u64) -> String {
    let sep = match language() {
        "en" => ',',
        _ => '.',
    };
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(c);
    }
    out
}
