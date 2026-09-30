//! Settings (Search, Library, Index) and the list of files that couldn't be read.

use std::rc::Rc;

use adw::prelude::*;
use filefind_core::Failure;
use gtk::{gio, glib, pango};

use crate::backend::Backend;
use crate::i18n::{fmt_count, tr};
use crate::window::display_path;

const RESULT_LIMITS: [usize; 4] = [50, 100, 200, 500];

fn switch_row(title: &str, subtitle: &str, active: bool, apply: impl Fn(bool) + 'static) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder().title(title).subtitle(subtitle).active(active).build();
    row.connect_active_notify(move |row| apply(row.is_active()));
    row
}

pub fn present(parent: &impl IsA<gtk::Widget>, backend: &Rc<Backend>) -> adw::PreferencesDialog {
    let settings = backend.settings();
    let dialog = adw::PreferencesDialog::builder().search_enabled(false).build();

    // Search
    let search = adw::PreferencesPage::builder().name("search").title(tr("Search")).icon_name("system-search-symbolic").build();
    let general = adw::PreferencesGroup::new();
    let mut names = vec![tr("System Default")];
    names.extend(crate::i18n::LANGUAGES.iter().map(|(_, name)| name.to_string()));
    let languages = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
    let language = adw::ComboRow::builder()
        .title(tr("Language"))
        .subtitle(tr("Takes effect the next time you open Filefind"))
        .model(&languages)
        .build();
    let current = settings.language.as_deref().and_then(|code| crate::i18n::LANGUAGES.iter().position(|(c, _)| *c == code));
    language.set_selected(current.map_or(0, |i| i as u32 + 1));
    let b = backend.clone();
    language.connect_selected_notify(move |row| {
        let code = (row.selected() as usize).checked_sub(1).and_then(|i| crate::i18n::LANGUAGES.get(i)).map(|(c, _)| c.to_string());
        b.update_settings(|s| s.language = code);
    });
    general.add(&language);
    search.add(&general);
    let matching = adw::PreferencesGroup::new();
    let b = backend.clone();
    matching.add(&switch_row(
        &tr("Match Word Forms"),
        &tr("Searching for “invoices” also finds “invoice”, in English and Spanish"),
        settings.word_forms,
        move |on| b.update_settings(|s| s.word_forms = on),
    ));
    let limits = gtk::StringList::new(&RESULT_LIMITS.map(|n| n.to_string()).iter().map(String::as_str).collect::<Vec<_>>());
    let results = adw::ComboRow::builder().title(tr("Results to Show")).model(&limits).build();
    results.set_selected(RESULT_LIMITS.iter().position(|&n| n == settings.max_results).unwrap_or(1) as u32);
    let b = backend.clone();
    results.connect_selected_notify(move |row| {
        let limit = RESULT_LIMITS[(row.selected() as usize).min(RESULT_LIMITS.len() - 1)];
        b.update_settings(|s| s.max_results = limit);
    });
    matching.add(&results);
    let tips = adw::ButtonRow::builder().title(tr("Search Tips")).end_icon_name("go-next-symbolic").action_name("win.search-tips").build();
    let tips_group = adw::PreferencesGroup::new();
    tips_group.add(&tips);
    search.add(&matching);
    search.add(&tips_group);

    // Library
    let library = adw::PreferencesPage::builder().name("library").title(tr("Library")).icon_name("folder-symbolic").build();
    let what = adw::PreferencesGroup::builder().title(tr("What to Index")).build();
    let b = backend.clone();
    what.add(&switch_row(
        &tr("Find Other Files by Name"),
        &tr("Photos, music, archives and other files that aren't documents"),
        settings.file_names,
        move |on| b.update_settings(|s| s.file_names = on),
    ));
    let b = backend.clone();
    what.add(&switch_row(
        &tr("Include Hidden Files"),
        &tr("Files and folders whose names start with a dot"),
        settings.hidden_files,
        move |on| b.update_settings(|s| s.hidden_files = on),
    ));
    let when = adw::PreferencesGroup::builder().title(tr("Background")).build();
    let b = backend.clone();
    when.add(&switch_row(
        &tr("Index in the Background"),
        &tr("Keep the index up to date while Filefind is closed, starting when you log in"),
        settings.background,
        move |on| b.update_settings(|s| s.background = on),
    ));
    library.add(&what);
    library.add(&excluded_folders_group(backend, &dialog));
    library.add(&excluded_names_group(backend, &dialog));
    library.add(&when);

    // Index
    let index = adw::PreferencesPage::builder().name("index").title(tr("Index")).icon_name("drive-harddisk-symbolic").build();
    let status = adw::PreferencesGroup::new();
    let files = adw::ActionRow::builder().title(tr("Files")).build();
    let files_value = value_label();
    files.add_suffix(&files_value);
    let size = adw::ActionRow::builder().title(tr("Size on Disk")).build();
    let size_value = value_label();
    size.add_suffix(&size_value);
    let failed = adw::ActionRow::builder()
        .title(tr("Files That Couldn't Be Read"))
        .activatable(true)
        .action_name("win.show-failures")
        .build();
    let failed_value = value_label();
    failed.add_suffix(&failed_value);
    failed.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    status.add(&files);
    status.add(&size);
    status.add(&failed);
    let rebuild_group = adw::PreferencesGroup::builder()
        .description(tr("Reads every file again. Only needed if results seem wrong."))
        .build();
    rebuild_group.add(&adw::ButtonRow::builder().title(tr("Rebuild Index")).action_name("win.rebuild").build());

    let advanced = adw::PreferencesGroup::builder().title(tr("Advanced")).build();
    let location = adw::ExpanderRow::builder().title(tr("Index Location")).build();
    let note = gtk::Label::builder()
        .label(tr("The index is stored with Filefind's other data. Move it only if you want it on another disk, for example a larger one."))
        .wrap(true)
        .xalign(0.0)
        .build();
    note.add_css_class("dim-label");
    note.add_css_class("location-note");
    let note_row = gtk::ListBoxRow::builder().child(&note).activatable(false).selectable(false).build();
    let change = adw::ButtonRow::builder().title(tr("Choose Another Location…")).build();
    let reset = adw::ButtonRow::builder().title(tr("Use Default Location")).build();
    location.add_row(&note_row);
    location.add_row(&change);
    location.add_row(&reset);
    advanced.add(&location);
    index.add(&status);
    index.add(&rebuild_group);
    index.add(&advanced);

    dialog.add(&search);
    dialog.add(&library);
    dialog.add(&index);

    let refresh = glib::clone!(#[weak] backend, #[weak] files_value, #[weak] size_value, #[weak] failed_value, #[weak] location, #[weak] reset, #[weak] change, move || {
        let (docs, failed) = match backend.state() {
            crate::backend::IndexState::Ready(s) => (Some(s.docs), Some(s.failed)),
            _ => (None, None),
        };
        files_value.set_label(&docs.map(fmt_count).unwrap_or_else(|| "…".into()));
        failed_value.set_label(&failed.map(fmt_count).unwrap_or_else(|| "…".into()));
        let dir = backend.index_dir();
        location.set_subtitle(&glib::markup_escape_text(&display_path(&dir)));
        reset.set_sensitive(dir != backend.default_index_dir() && !backend.is_moving());
        change.set_sensitive(!backend.is_moving());
        glib::spawn_future_local(glib::clone!(#[weak] backend, #[weak] size_value, async move {
            size_value.set_label(&glib::format_size(backend.index_size().await));
        }));
    });
    refresh();
    let subscription = backend.subscribe(refresh);
    let b = backend.clone();
    dialog.connect_closed(move |_| b.unsubscribe(subscription));

    let move_to = glib::clone!(#[weak] backend, #[weak] dialog, move |target: Option<std::path::PathBuf>| {
        glib::spawn_future_local(async move {
            let message = match backend.clone().move_index(target).await {
                Ok(()) => tr("Index moved"),
                Err(e) => e,
            };
            dialog.add_toast(adw::Toast::new(&message));
        });
    });
    let move_to = Rc::new(move_to);
    change.connect_activated(glib::clone!(#[weak] dialog, #[strong] move_to, move |_| {
        let chooser = gtk::FileDialog::builder().title(tr("Choose Where to Keep the Index")).modal(true).build();
        let window = dialog.root().and_downcast::<gtk::Window>();
        let move_to = move_to.clone();
        chooser.select_folder(window.as_ref(), gio::Cancellable::NONE, move |res| {
            if let Some(path) = res.ok().and_then(|f| f.path()) {
                move_to(Some(path));
            }
        });
    }));
    reset.connect_activated(move |_| move_to(None));

    dialog.present(Some(parent));
    dialog
}

/// A group whose rows are rebuilt from the settings whenever they change.
fn live_group(backend: &Rc<Backend>, group: &adw::PreferencesGroup, dialog: &adw::PreferencesDialog, rows: impl Fn(&Rc<Backend>) -> Vec<gtk::Widget> + 'static) {
    let shown: Rc<std::cell::RefCell<Vec<gtk::Widget>>> = Rc::default();
    let refresh = glib::clone!(#[weak] backend, #[weak] group, #[strong] shown, move || {
        for row in shown.borrow_mut().drain(..) {
            group.remove(&row);
        }
        for row in rows(&backend) {
            group.add(&row);
            shown.borrow_mut().push(row);
        }
    });
    refresh();
    let subscription = backend.subscribe(refresh);
    let b = backend.clone();
    dialog.connect_closed(move |_| b.unsubscribe(subscription));
}

fn removable_row(title: &str, subtitle: Option<&str>, icon: Option<&str>, remove: impl Fn() + 'static) -> gtk::Widget {
    let row = adw::ActionRow::builder().title(glib::markup_escape_text(title)).build();
    if let Some(subtitle) = subtitle {
        row.set_subtitle(&glib::markup_escape_text(subtitle));
    }
    if let Some(icon) = icon {
        row.add_prefix(&gtk::Image::from_icon_name(icon));
    }
    let button = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text(tr("Remove")).valign(gtk::Align::Center).build();
    button.add_css_class("flat");
    button.connect_clicked(move |_| remove());
    row.add_suffix(&button);
    row.upcast()
}

fn placeholder_row(text: &str) -> gtk::Widget {
    let row = adw::ActionRow::builder().title(text).build();
    row.add_css_class("dim-label");
    row.upcast()
}

fn excluded_folders_group(backend: &Rc<Backend>, dialog: &adw::PreferencesDialog) -> adw::PreferencesGroup {
    let add = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text(tr("Exclude a Folder…")).valign(gtk::Align::Center).build();
    add.add_css_class("flat");
    let group = adw::PreferencesGroup::builder()
        .title(tr("Excluded Folders"))
        .description(tr("Nothing inside these folders is indexed."))
        .header_suffix(&add)
        .build();
    add.connect_clicked(glib::clone!(#[weak] backend, move |button| {
        choose_excluded_folder(&backend, button.root().and_downcast_ref::<gtk::Window>(), None);
    }));
    live_group(backend, &group, dialog, |backend| {
        let folders = backend.settings().excluded_folders;
        if folders.is_empty() {
            return vec![placeholder_row(&tr("No excluded folders"))];
        }
        folders
            .into_iter()
            .map(|folder| {
                let b = backend.clone();
                let target = folder.clone();
                removable_row(&crate::window::display_name(&folder), Some(&display_path(&folder)), Some("folder-symbolic"), move || {
                    b.update_settings(|s| s.excluded_folders.retain(|f| f != &target))
                })
            })
            .collect()
    });
    group
}

/// Asks for a folder to exclude, starting in `start` (e.g. a library folder).
pub fn choose_excluded_folder(backend: &Rc<Backend>, parent: Option<&gtk::Window>, start: Option<&std::path::Path>) {
    let chooser = gtk::FileDialog::builder().title(tr("Exclude a Folder")).accept_label(tr("Exclude")).modal(true).build();
    if let Some(start) = start {
        chooser.set_initial_folder(Some(&gio::File::for_path(start)));
    }
    let backend = backend.clone();
    chooser.select_folder(parent, gio::Cancellable::NONE, move |res| {
        let Some(path) = res.ok().and_then(|f| f.path()) else { return };
        backend.update_settings(|s| {
            if !s.excluded_folders.contains(&path) {
                s.excluded_folders.push(path);
                s.excluded_folders.sort();
            }
        });
    });
}

fn excluded_names_group(backend: &Rc<Backend>, dialog: &adw::PreferencesDialog) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(tr("Excluded Names"))
        .description(tr("Files and folders with a matching name are skipped wherever they are. Use * as a wildcard, as in *.log or draft-*."))
        .build();
    let entry = adw::EntryRow::builder().title(tr("Add a name or pattern")).show_apply_button(true).build();
    entry.connect_apply(glib::clone!(#[weak] backend, #[weak] dialog, move |entry| {
        let pattern = entry.text().trim().to_owned();
        if pattern.is_empty() {
            return;
        }
        if pattern.contains('/') {
            dialog.add_toast(adw::Toast::new(&tr("To skip a folder by its location, add it under Excluded Folders.")));
            return;
        }
        backend.update_settings(|s| {
            if !s.excluded_names.contains(&pattern) {
                s.excluded_names.push(pattern);
            }
        });
        entry.set_text("");
    }));
    group.add(&entry);
    live_group(backend, &group, dialog, |backend| {
        backend
            .settings()
            .excluded_names
            .into_iter()
            .map(|pattern| {
                let b = backend.clone();
                let target = pattern.clone();
                removable_row(&pattern, None, None, move || b.update_settings(|s| s.excluded_names.retain(|p| p != &target)))
            })
            .collect()
    });
    group
}

fn value_label() -> gtk::Label {
    let label = gtk::Label::new(None);
    label.add_css_class("dim-label");
    label.add_css_class("numeric");
    label
}

fn failure_text(failure: Failure) -> String {
    match failure {
        Failure::Encrypted => tr("Password protected"),
        Failure::TimedOut => tr("Took too long to read"),
        Failure::Crashed => tr("Couldn't be read safely"),
        Failure::Unreadable => tr("Damaged or unsupported"),
    }
}

pub fn present_failures(parent: &impl IsA<gtk::Widget>, backend: &Rc<Backend>) {
    let dialog = adw::Dialog::builder().title(tr("Files That Couldn't Be Read")).content_width(560).content_height(520).build();
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let intro = gtk::Label::builder()
        .label(tr("These files are still found by name, but their contents can't be searched."))
        .wrap(true)
        .xalign(0.0)
        .build();
    intro.add_css_class("dim-label");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.add_css_class("failures");
    content.append(&intro);
    content.append(&list);
    let clamp = adw::Clamp::builder().child(&content).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build()));
    dialog.set_child(Some(&view));

    glib::spawn_future_local(glib::clone!(#[weak] backend, #[weak] list, async move {
        for file in backend.failed_files().await {
            let path = std::path::Path::new(&file.path);
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&crate::window::display_name(path)))
                .subtitle(glib::markup_escape_text(&failure_text(file.failure)))
                .tooltip_text(display_path(path))
                .activatable(true)
                .build();
            row.set_title_lines(1);
            let reveal = gtk::Button::builder().icon_name("folder-open-symbolic").tooltip_text(tr("Show in Folder")).valign(gtk::Align::Center).build();
            reveal.add_css_class("flat");
            let target = file.path.clone();
            reveal.connect_clicked(move |b| crate::launch::show_in_folder(&target, b.root().and_downcast_ref::<gtk::Window>(), || {}));
            row.add_suffix(&reveal);
            row.connect_activated(move |r| crate::launch::open(&file.path, r.root().and_downcast_ref::<gtk::Window>(), || {}));
            list.append(&row);
        }
        if list.row_at_index(0).is_none() {
            let label = gtk::Label::builder().label(tr("Every file was read successfully.")).margin_top(12).margin_bottom(12).build();
            label.set_ellipsize(pango::EllipsizeMode::End);
            list.append(&label);
        }
    }));
    dialog.present(Some(parent));
}
