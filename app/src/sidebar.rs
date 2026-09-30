//! The library sidebar: folders to search in, each folder's options, and indexing status.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use filefind_core::{Folder, Progress};
use gtk::{glib, pango};

use crate::backend::{Backend, IndexState};
use crate::i18n::{fmt_count, ntr, tr};
use crate::window::{display_name, display_path};

/// Library folders, their file counts once known, and the excluded folders.
type ShownFolders = (Vec<Folder>, Option<Vec<u64>>, Vec<PathBuf>);
type ScopeCallback = Rc<dyn Fn(Option<Folder>)>;

pub struct Sidebar {
    pub widget: adw::ToolbarView,
    backend: Rc<Backend>,
    list: gtk::ListBox,
    all_count: gtk::Label,
    /// The folders listed after "All Folders", and their counts; rows are rebuilt only when
    /// this changes.
    shown: RefCell<Option<ShownFolders>>,
    status: StatusView,
    on_scope: RefCell<Option<ScopeCallback>>,
    refreshing: Cell<bool>,
}

impl Sidebar {
    pub fn new(backend: &Rc<Backend>) -> Rc<Sidebar> {
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new(&tr("Library"), "")));
        let add = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text(tr("Add Folder…")).action_name("win.add-folder").build();
        header.pack_end(&add);

        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        let all_count = gtk::Label::new(None);
        all_count.add_css_class("sidebar-count");
        list.append(&sidebar_row("folder-saved-search-symbolic", &tr("All Folders"), None, &all_count, None));
        let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&list).vexpand(true).build();

        let add_button = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label(tr("Add Folder…")).build())
            .action_name("win.add-folder")
            .build();
        add_button.add_css_class("sidebar-add");
        let status = StatusView::new();
        let footer = gtk::Box::new(gtk::Orientation::Vertical, 10);
        footer.add_css_class("sidebar-footer");
        footer.append(&status.widget);
        footer.append(&add_button);

        let widget = adw::ToolbarView::new();
        widget.add_top_bar(&header);
        widget.set_content(Some(&scroller));
        widget.add_bottom_bar(&footer);

        let sidebar = Rc::new(Sidebar {
            widget,
            backend: backend.clone(),
            list: list.clone(),
            all_count,
            shown: RefCell::new(None),
            status,
            on_scope: RefCell::new(None),
            refreshing: Cell::new(false),
        });
        list.connect_row_selected(glib::clone!(#[weak] sidebar, move |_, row| {
            if sidebar.refreshing.get() {
                return;
            }
            let scope = row.and_then(|r| sidebar.folder_at(r.index()));
            let callback = sidebar.on_scope.borrow().clone();
            if let Some(f) = callback {
                f(scope);
            }
        }));
        sidebar.refresh();
        sidebar
    }

    /// Called with the folder to search in (`None`: all folders) when the selection changes.
    pub fn connect_scope_changed(&self, f: impl Fn(Option<Folder>) + 'static) {
        self.on_scope.replace(Some(Rc::new(f)));
    }

    fn folder_at(&self, index: i32) -> Option<Folder> {
        let i = usize::try_from(index).ok()?.checked_sub(1)?;
        self.shown.borrow().as_ref()?.0.get(i).cloned()
    }

    pub fn scope(&self) -> Option<Folder> {
        self.list.selected_row().and_then(|r| self.folder_at(r.index()))
    }

    pub fn refresh(self: &Rc<Self>) {
        let library = self.backend.library();
        let state = self.backend.state();
        let counts = match &state {
            IndexState::Ready(status) if status.folders.len() == library.folders.len() => Some(status.folders.clone()),
            _ => None,
        };
        self.status.update(&state);
        let excluded = self.backend.settings().excluded_folders;
        let shown = Some((library.folders.clone(), counts.clone(), excluded.clone()));
        let same_folders = self.shown.borrow().as_ref().is_some_and(|s| s.0 == library.folders && s.2 == excluded);
        if *self.shown.borrow() == shown || (counts.is_none() && same_folders) {
            return;
        }
        let selected = self.scope().map(|f| f.path);
        self.shown.replace(shown);

        self.refreshing.set(true);
        while let Some(row) = self.list.row_at_index(1) {
            self.list.remove(&row);
        }
        for (i, folder) in library.folders.iter().enumerate() {
            let inside = excluded_inside(folder, &excluded);
            self.list.append(&self.folder_row(folder, counts.as_ref().and_then(|c| c.get(i)).copied(), &inside));
        }
        let position = selected.as_ref().and_then(|p| library.folders.iter().position(|f| &f.path == p));
        self.list.select_row(self.list.row_at_index(position.map_or(0, |i| i as i32 + 1)).as_ref());
        self.refreshing.set(false);
        // The folder being searched was removed: search everywhere again.
        if selected.is_some() && position.is_none() {
            let callback = self.on_scope.borrow().clone();
            if let Some(f) = callback {
                f(None);
            }
        }

        let total = counts.map(|c| c.iter().sum::<u64>());
        self.all_count.set_label(&total.map(fmt_count).unwrap_or_default());
    }

    fn folder_row(self: &Rc<Self>, folder: &Folder, count: Option<u64>, excluded: &[PathBuf]) -> gtk::ListBoxRow {
        let available = folder.path.is_dir();
        let subtitle = if folder.include_subfolders { tr("Includes subfolders") } else { tr("This folder only") };
        let subtitle = if available { subtitle } else { tr("Not available") };
        let count_label = gtk::Label::new(count.map(fmt_count).as_deref());
        count_label.add_css_class("sidebar-count");

        let menu = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text(tr("Folder Options"))
            .valign(gtk::Align::Center)
            .popover(&self.folder_popover(folder, excluded))
            .build();
        menu.add_css_class("flat");
        menu.add_css_class("circular");
        menu.add_css_class("sidebar-menu");

        // A crossed-out eye tells that some subfolders are left out; the tooltip says which.
        let counts = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        if !excluded.is_empty() {
            let n = excluded.len() as u64;
            let hidden = gtk::Image::from_icon_name("view-conceal-symbolic");
            hidden.add_css_class("sidebar-excluded");
            hidden.set_tooltip_text(Some(&ntr("{} excluded", "{} excluded", n).replace("{}", &n.to_string())));
            counts.append(&hidden);
        }
        counts.append(&count_label);
        let icon = if available { "folder-symbolic" } else { "folder-remote-symbolic" };
        let row = sidebar_row(icon, &display_name(&folder.path), Some(&subtitle), &counts, Some(&menu));
        let mut tooltip = display_path(&folder.path);
        if !excluded.is_empty() {
            tooltip.push_str(&format!("\n\n{}", tr("Excluded:")));
            for path in excluded {
                tooltip.push_str(&format!("\n{}", relative(folder, path)));
            }
        }
        row.set_tooltip_text(Some(&tooltip));
        if !available {
            row.add_css_class("unavailable");
        }
        row
    }

    fn excluded_row(self: &Rc<Self>, folder: &Folder, path: &Path) -> gtk::Box {
        let label = gtk::Label::builder()
            .label(relative(folder, path))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::Middle)
            .tooltip_text(display_path(path))
            .build();
        let remove = gtk::Button::builder().icon_name("window-close-symbolic").tooltip_text(tr("Stop Excluding")).build();
        remove.add_css_class("flat");
        remove.add_css_class("circular");
        let target = path.to_owned();
        remove.connect_clicked(glib::clone!(#[weak(rename_to = sidebar)] self, move |button| {
            if let Some(popover) = button.ancestor(gtk::Popover::static_type()).and_downcast::<gtk::Popover>() {
                popover.popdown();
            }
            sidebar.backend.update_settings(|s| s.excluded_folders.retain(|f| f != &target));
        }));
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.add_css_class("excluded-row");
        row.append(&gtk::Image::from_icon_name("folder-symbolic"));
        row.append(&label);
        row.append(&remove);
        row
    }

    fn folder_popover(self: &Rc<Self>, folder: &Folder, excluded: &[PathBuf]) -> gtk::Popover {
        let name = display_name(&folder.path);
        let title = gtk::Label::builder().label(&name).xalign(0.0).ellipsize(pango::EllipsizeMode::Middle).build();
        title.add_css_class("heading");
        let location = gtk::Label::builder().label(display_path(&folder.path)).xalign(0.0).ellipsize(pango::EllipsizeMode::Middle).build();
        location.add_css_class("dim-label");
        location.add_css_class("caption");

        let switch = gtk::Switch::builder().active(folder.include_subfolders).valign(gtk::Align::Center).build();
        let switch_label = gtk::Label::builder().label(tr("Include subfolders")).xalign(0.0).hexpand(true).build();
        let switch_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        switch_row.append(&switch_label);
        switch_row.append(&switch);
        let hint = gtk::Label::builder()
            .label(tr("Also search every folder inside “{}”.").replace("{}", &name))
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(28)
            .build();
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");

        let exclude = menu_item("view-conceal-symbolic", &tr("Exclude a Subfolder…"));
        let reveal = menu_item("folder-open-symbolic", &tr("Open in File Manager"));
        let remove = menu_item("user-trash-symbolic", &tr("Remove from Library"));
        remove.add_css_class("destructive-text");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.add_css_class("folder-popover");
        content.append(&title);
        content.append(&location);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&switch_row);
        content.append(&hint);
        if !excluded.is_empty() {
            content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            let heading = gtk::Label::builder().label(tr("Excluded here")).xalign(0.0).build();
            heading.add_css_class("caption-heading");
            heading.add_css_class("dim-label");
            content.append(&heading);
            for path in excluded {
                content.append(&self.excluded_row(folder, path));
            }
        }
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&exclude);
        content.append(&reveal);
        content.append(&remove);
        let popover = gtk::Popover::builder().child(&content).build();

        let path = folder.path.clone();
        switch.connect_state_set(glib::clone!(#[weak(rename_to = sidebar)] self, #[weak] popover, #[strong] path, #[upgrade_or] glib::Propagation::Proceed, move |_, include| {
            // The rows are rebuilt with the new setting, so close the menu first.
            popover.popdown();
            sidebar.backend.update_library(|lib| lib.set_include_subfolders(&path, include));
            glib::Propagation::Proceed
        }));
        exclude.connect_clicked(glib::clone!(#[weak(rename_to = sidebar)] self, #[weak] popover, #[strong] path, move |_| {
            popover.popdown();
            let window = popover.root().and_downcast::<gtk::Window>();
            crate::preferences::choose_excluded_folder(&sidebar.backend, window.as_ref(), Some(&path));
        }));
        reveal.connect_clicked(glib::clone!(#[weak] popover, #[strong] path, move |_| {
            popover.popdown();
            crate::launch::open(&path.to_string_lossy(), popover.root().and_downcast_ref::<gtk::Window>(), || {});
        }));
        remove.connect_clicked(glib::clone!(#[weak] popover, move |b| {
            popover.popdown();
            let _ = b.activate_action("win.remove-folder", Some(&path.to_string_lossy().to_variant()));
        }));
        popover
    }
}

/// Excluded folders below `folder` that would otherwise be indexed.
fn excluded_inside(folder: &Folder, excluded: &[PathBuf]) -> Vec<PathBuf> {
    if !folder.include_subfolders {
        return Vec::new();
    }
    excluded.iter().filter(|p| p.starts_with(&folder.path) && **p != folder.path).cloned().collect()
}

fn relative(folder: &Folder, path: &Path) -> String {
    path.strip_prefix(&folder.path).map(|r| r.display().to_string()).unwrap_or_else(|_| display_path(path))
}

/// A flat, left-aligned button with an icon, like an item in a menu.
fn menu_item(icon: &str, label: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&gtk::Label::new(Some(label)));
    let button = gtk::Button::builder().child(&content).build();
    button.add_css_class("flat");
    button.add_css_class("menu-item");
    button
}

fn sidebar_row(
    icon: &str,
    title: &str,
    subtitle: Option<&str>,
    count: &impl IsA<gtk::Widget>,
    suffix: Option<&gtk::MenuButton>,
) -> gtk::ListBoxRow {
    let title_label = gtk::Label::builder().label(title).xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    text.append(&title_label);
    if let Some(subtitle) = subtitle {
        let label = gtk::Label::builder().label(subtitle).xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
        label.add_css_class("sidebar-subtitle");
        text.append(&label);
    }
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&text);
    content.append(count);
    if let Some(suffix) = suffix {
        content.append(suffix);
    }
    gtk::ListBoxRow::builder().child(&content).build()
}

/// Indexing progress, or a green check once everything is indexed.
struct StatusView {
    widget: gtk::Box,
    icon: gtk::Stack,
    title: gtk::Label,
    detail: gtk::Label,
    percent: gtk::Label,
    bar: gtk::ProgressBar,
    failures: gtk::Button,
    failures_label: gtk::Label,
    /// When the current indexing run started, and how much was done then, for the estimate.
    run_start: Cell<Option<(Instant, usize)>>,
}

impl StatusView {
    fn new() -> StatusView {
        // Fixed-size and centered, so the badge stays round next to two lines of text.
        let icon = gtk::Stack::builder().valign(gtk::Align::Center).halign(gtk::Align::Center).width_request(20).height_request(20).build();
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.add_css_class("status-ok");
        check.set_valign(gtk::Align::Center);
        check.set_halign(gtk::Align::Center);
        let warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
        warning.add_css_class("status-warning");
        icon.add_named(&check, Some("ok"));
        icon.add_named(&adw::Spinner::new(), Some("working"));
        icon.add_named(&warning, Some("warning"));

        let title = gtk::Label::builder().xalign(0.0).build();
        title.add_css_class("status-title");
        let detail = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
        detail.add_css_class("status-detail");
        let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
        texts.set_hexpand(true);
        texts.append(&title);
        texts.append(&detail);
        let percent = gtk::Label::builder().valign(gtk::Align::Center).build();
        percent.add_css_class("status-percent");
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        heading.append(&icon);
        heading.append(&texts);
        heading.append(&percent);

        let bar = gtk::ProgressBar::builder().pulse_step(0.08).build();
        bar.add_css_class("status-progress");
        let failures_label = gtk::Label::builder().ellipsize(pango::EllipsizeMode::End).build();
        let failures_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        failures_content.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        failures_content.append(&failures_label);
        let failures = gtk::Button::builder().child(&failures_content).action_name("win.show-failures").halign(gtk::Align::Start).build();
        failures.add_css_class("flat");
        failures.add_css_class("status-failures");

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 6);
        widget.add_css_class("status-view");
        widget.append(&heading);
        widget.append(&bar);
        widget.append(&failures);
        StatusView { widget, icon, title, detail, percent, bar, failures, failures_label, run_start: Cell::new(None) }
    }

    fn update(&self, state: &IndexState) {
        self.update_texts(state);
        self.detail.set_visible(!self.detail.label().is_empty());
    }

    fn update_texts(&self, state: &IndexState) {
        // While folders are still being scanned the total keeps growing, so a percentage
        // would hover near 100%: show an indeterminate bar instead.
        let fraction = match state {
            IndexState::Working(p) if p.total > 0 && !p.scanning => Some(p.done as f64 / p.total as f64),
            _ => None,
        };
        self.bar.set_visible(matches!(state, IndexState::Working(p) if p.total > 0));
        self.percent.set_visible(fraction.is_some());
        match fraction {
            Some(fraction) => {
                self.bar.set_fraction(fraction);
                self.percent.set_label(&format!("{}%", (fraction * 100.0).floor()));
            }
            None => self.bar.pulse(),
        }
        self.failures.set_visible(matches!(state, IndexState::Ready(s) if s.failed > 0));
        match state {
            IndexState::Working(p) => {
                self.icon.set_visible_child_name("working");
                if p.total == 0 {
                    self.title.set_label(&tr("Looking for files…"));
                    self.detail.set_label(&ntr("{} file checked", "{} files checked", p.scanned as u64).replace("{}", &fmt_count(p.scanned as u64)));
                } else if p.scanning {
                    self.title.set_label(&tr("Indexing…"));
                    self.detail.set_label(&ntr("{} file indexed", "{} files indexed", p.done as u64).replace("{}", &fmt_count(p.done as u64)));
                } else {
                    self.title.set_label(&tr("Indexing…"));
                    self.detail.set_label(&self.progress_detail(p));
                }
            }
            IndexState::Ready(status) => {
                self.run_start.set(None);
                self.icon.set_visible_child_name("ok");
                self.title.set_label(&tr("Up to date"));
                self.detail.set_label(&ntr("{} file indexed", "{} files indexed", status.docs).replace("{}", &fmt_count(status.docs)));
                self.failures_label.set_label(
                    &ntr("{} file couldn't be read", "{} files couldn't be read", status.failed).replace("{}", &fmt_count(status.failed)),
                );
            }
            IndexState::Stopped => {
                self.icon.set_visible_child_name("working");
                self.title.set_label(&tr("Starting…"));
                self.detail.set_label("");
            }
            IndexState::Broken => {
                self.icon.set_visible_child_name("warning");
                self.title.set_label(&tr("Index unavailable"));
                self.detail.set_label(&tr("Try Rebuild Index in Settings"));
            }
        }
    }

    fn progress_detail(&self, p: &Progress) -> String {
        let counts = tr("{done} of {total}")
            .replace("{done}", &fmt_count(p.done as u64))
            .replace("{total}", &fmt_count(p.total as u64));
        let (started, done_then) = match self.run_start.get() {
            Some(start) if start.1 <= p.done => start,
            _ => {
                self.run_start.set(Some((Instant::now(), p.done)));
                return counts;
            }
        };
        let elapsed = started.elapsed().as_secs_f64();
        let rate = (p.done - done_then) as f64 / elapsed.max(0.001);
        // Estimates are noisy at first.
        if elapsed < 3.0 || rate <= 0.0 {
            return counts;
        }
        let minutes = ((p.total - p.done) as f64 / rate / 60.0).ceil() as u64;
        let left = if minutes <= 1 {
            tr("less than a minute left")
        } else {
            tr("about {} minutes left").replace("{}", &minutes.to_string())
        };
        format!("{counts} · {left}")
    }
}
