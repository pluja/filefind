//! The main window: library sidebar, search field and filters, results, and quick preview.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use filefind_core::{AddOutcome, Filters, Hit, SearchResults};
use gtk::{gdk, gio, glib};

use crate::backend::{Backend, IndexState};
use crate::filters::FilterBar;
use crate::i18n::{fmt_count, ntr, tr};
use crate::preview::Preview;
use crate::sidebar::Sidebar;
use crate::{APP_ID, APP_NAME, VERSION};

thread_local! {
    static CURRENT: RefCell<Weak<State>> = const { RefCell::new(Weak::new()) };
}

pub struct State {
    window: adw::ApplicationWindow,
    backend: Rc<Backend>,
    toasts: adw::ToastOverlay,
    library_split: adw::OverlaySplitView,
    preview_split: adw::OverlaySplitView,
    sidebar: Rc<Sidebar>,
    filters: Rc<FilterBar>,
    preview: Rc<Preview>,
    search: gtk::SearchEntry,
    stack: gtk::Stack,
    list: gtk::ListBox,
    count: gtk::Label,
    ready_page: adw::StatusPage,
    status_revealer: gtk::Revealer,
    status_label: gtk::Label,
    scroller: gtk::ScrolledWindow,
    context_menu: gtk::PopoverMenu,
    /// Whether the user asked for the preview; the split view may not show or hide it on its own.
    preview_wanted: Cell<bool>,
    generation: Cell<u64>,
    hits: RefCell<Vec<Hit>>,
    terms: RefCell<Vec<String>>,
    was_working: Cell<bool>,
    last_refresh: Cell<Option<std::time::Instant>>,
}

/// Shows the main window, creating it if needed, optionally searching for `query`.
pub fn present(app: &adw::Application, backend: &Rc<Backend>, query: Option<&str>) {
    let state = CURRENT.with(|c| c.borrow().upgrade()).unwrap_or_else(|| build(app, backend));
    backend.start_indexing();
    if let Some(query) = query {
        state.search.set_text(query);
        state.search.set_position(-1);
    }
    state.window.present();
}

fn build(app: &adw::Application, backend: &Rc<Backend>) -> Rc<State> {
    let saved = backend.settings().window;
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(APP_NAME)
        .default_width(saved.width)
        .default_height(saved.height)
        .maximized(saved.maximized)
        .width_request(360)
        .height_request(420)
        .build();

    let header = adw::HeaderBar::new();
    header.set_show_title(false);
    let sidebar_toggle = gtk::ToggleButton::builder().icon_name("sidebar-show-symbolic").tooltip_text(tr("Library")).build();
    header.pack_start(&sidebar_toggle);
    let menu = gio::Menu::new();
    let section = gio::Menu::new();
    section.append(Some(&tr("_Add Folder…")), Some("win.add-folder"));
    section.append(Some(&tr("_Settings")), Some("win.settings"));
    menu.append_section(None, &section);
    let help = gio::Menu::new();
    help.append(Some(&tr("_Search Tips")), Some("win.search-tips"));
    help.append(Some(&tr("_Keyboard Shortcuts")), Some("win.shortcuts"));
    help.append(Some(&tr("_About Filefind")), Some("win.about"));
    menu.append_section(None, &help);
    header.pack_end(
        &gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text(tr("Main Menu")).primary(true).build(),
    );
    let preview_toggle = gtk::ToggleButton::builder().icon_name("view-reveal-symbolic").tooltip_text(tr("Quick Preview (Space)")).build();
    header.pack_end(&preview_toggle);

    let search = gtk::SearchEntry::builder().placeholder_text(tr("Search files and documents")).hexpand(true).search_delay(40).build();
    search.add_css_class("hero-search");
    let tips_button = gtk::Button::builder()
        .icon_name("help-about-symbolic")
        .tooltip_text(tr("Search Tips"))
        .action_name("win.search-tips")
        .valign(gtk::Align::Center)
        .build();
    tips_button.add_css_class("flat");
    tips_button.add_css_class("circular");
    let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    search_row.append(&search);
    search_row.append(&tips_button);
    let filters = FilterBar::new(backend);
    let search_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    search_box.append(&search_row);
    search_box.append(&filters.widget);
    let search_bar = adw::Clamp::builder().maximum_size(960).tightening_threshold(700).child(&search_box).build();
    search_bar.add_css_class("search-bar");

    let welcome = adw::StatusPage::builder()
        .icon_name(APP_ID)
        .title(tr("Welcome to Filefind"))
        .description(tr("Add the folders you want to search. Filefind reads your documents — PDF, Word, LibreOffice, text and more — so you can find anything by what's inside."))
        .build();
    let add_button = gtk::Button::builder().label(tr("Add Folder…")).halign(gtk::Align::Center).action_name("win.add-folder").build();
    add_button.add_css_class("pill");
    add_button.add_css_class("suggested-action");
    welcome.set_child(Some(&add_button));

    let ready_page = adw::StatusPage::builder().icon_name("system-search-symbolic").title(tr("Search Your Files")).build();
    let tips_link = gtk::Button::builder()
        .label(tr("Try type:pdf, in:folder or -word — Search Tips"))
        .action_name("win.search-tips")
        .halign(gtk::Align::Center)
        .build();
    tips_link.add_css_class("flat");
    tips_link.add_css_class("tips-link");
    ready_page.set_child(Some(&tips_link));
    let empty = adw::StatusPage::builder()
        .icon_name("edit-find-symbolic")
        .title(tr("No Results Found"))
        .description(tr("Check the spelling, try other words, or clear the filters."))
        .build();

    let list = gtk::ListBox::new();
    list.add_css_class("results");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_activate_on_single_click(false);
    let count = gtk::Label::builder().xalign(0.0).build();
    count.add_css_class("results-count");
    let results_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    results_box.append(&count);
    results_box.append(&list);
    let context_menu = crate::results::context_menu(&results_box);
    let clamp = adw::Clamp::builder()
        .maximum_size(860)
        .tightening_threshold(600)
        .child(&results_box)
        .margin_start(6)
        .margin_end(6)
        .margin_bottom(12)
        .build();
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).vexpand(true).build();

    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).transition_duration(120).build();
    stack.add_named(&welcome, Some("welcome"));
    stack.add_named(&ready_page, Some("ready"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&scroller, Some("results"));

    // Compact indexing status for when the library sidebar is hidden.
    let status_label = gtk::Label::new(None);
    let status_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_box.set_halign(gtk::Align::Center);
    status_box.append(&adw::Spinner::new());
    status_box.append(&status_label);
    status_box.add_css_class("status-bar");
    let status_revealer = gtk::Revealer::builder().child(&status_box).transition_type(gtk::RevealerTransitionType::SlideUp).build();

    let content = adw::ToolbarView::new();
    content.add_top_bar(&header);
    content.add_top_bar(&search_bar);
    content.set_content(Some(&stack));
    content.add_bottom_bar(&status_revealer);

    let preview = Preview::new(backend);
    let preview_split = adw::OverlaySplitView::builder()
        .content(&content)
        .sidebar_position(gtk::PackType::End)
        .show_sidebar(false)
        // Only the user opens the preview; don't show it just because the window got wider.
        .pin_sidebar(true)
        .build();
    crate::resize::set_width(&preview_split, saved.preview_width);
    let b = backend.clone();
    preview_split.set_sidebar(Some(&crate::resize::resizable(&preview_split, &preview.widget, move |w| {
        b.update_settings(|s| s.window.preview_width = w)
    })));
    preview_split.bind_property("show-sidebar", &preview_toggle, "active").sync_create().build();

    let sidebar = Sidebar::new(backend);
    let library_split = adw::OverlaySplitView::builder().content(&preview_split).show_sidebar(saved.sidebar).build();
    crate::resize::set_width(&library_split, saved.sidebar_width);
    let b = backend.clone();
    library_split.set_sidebar(Some(&crate::resize::resizable(&library_split, &sidebar.widget, move |w| {
        b.update_settings(|s| s.window.sidebar_width = w)
    })));
    library_split.bind_property("show-sidebar", &sidebar_toggle, "active").bidirectional().sync_create().build();

    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(adw::BreakpointConditionLengthType::MaxWidth, 640.0, adw::LengthUnit::Sp));
    narrow.add_setter(&library_split, "collapsed", Some(&true.to_value()));
    narrow.add_setter(&library_split, "show-sidebar", Some(&false.to_value()));
    window.add_breakpoint(narrow);
    let medium = adw::Breakpoint::new(adw::BreakpointCondition::new_length(adw::BreakpointConditionLengthType::MaxWidth, 1000.0, adw::LengthUnit::Sp));
    medium.add_setter(&preview_split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(medium);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&library_split));
    window.set_content(Some(&toasts));

    let state = Rc::new(State {
        window: window.clone(),
        backend: backend.clone(),
        toasts,
        library_split: library_split.clone(),
        preview_split: preview_split.clone(),
        sidebar: sidebar.clone(),
        filters: filters.clone(),
        preview,
        search: search.clone(),
        stack,
        list: list.clone(),
        count,
        ready_page,
        status_revealer,
        status_label,
        scroller,
        context_menu,
        preview_wanted: Cell::new(false),
        generation: Cell::new(0),
        hits: RefCell::new(Vec::new()),
        terms: RefCell::new(Vec::new()),
        was_working: Cell::new(false),
        last_refresh: Cell::new(None),
    });
    CURRENT.with(|c| c.replace(Rc::downgrade(&state)));

    wire_search(&state);
    wire_results(&state);
    install_actions(&state);
    filters.connect_changed(glib::clone!(#[weak] state, move || state.run_search()));
    sidebar.connect_scope_changed(glib::clone!(#[weak] state, move |_| state.run_search()));
    library_split.connect_show_sidebar_notify(glib::clone!(#[weak] state, move |_| state.update_status()));
    preview_toggle.connect_clicked(glib::clone!(#[weak] state, move |toggle| state.set_preview(toggle.is_active())));
    preview_split.connect_show_sidebar_notify(glib::clone!(#[weak] state, move |split| {
        let wanted = state.preview_wanted.get();
        if split.shows_sidebar() && !wanted {
            split.set_show_sidebar(false);
        } else if split.shows_sidebar() {
            state.preview_selected();
        } else if split.is_collapsed() {
            // Dismissed in overlay mode (click outside or swipe).
            state.preview_wanted.set(false);
        }
    }));

    // Drop folders anywhere on the window to add them.
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    drop.connect_drop(glib::clone!(#[weak] state, #[upgrade_or] false, move |_, value, _, _| {
        let Ok(files) = value.get::<gdk::FileList>() else { return false };
        let folders: Vec<PathBuf> = files.files().iter().filter_map(|f| f.path()).filter(|p| p.is_dir()).collect();
        if folders.is_empty() {
            state.toast(&tr("Drop folders here to add them to your library"));
            return false;
        }
        state.add_folders(folders);
        true
    }));
    window.add_controller(drop);

    let subscription = backend.subscribe(glib::clone!(#[weak] state, move || state.backend_changed()));
    window.connect_close_request(glib::clone!(#[strong] state, move |window| {
        let (width, height) = window.default_size();
        let sidebar = state.library_split.is_collapsed() || state.library_split.shows_sidebar();
        state.backend.update_settings(|s| {
            s.window = crate::settings::WindowState { width, height, maximized: window.is_maximized(), sidebar, ..s.window.clone() };
        });
        state.backend.unsubscribe(subscription);
        CURRENT.with(|c| c.replace(Weak::new()));
        if !state.backend.settings().background {
            state.backend.stop_indexing();
        }
        glib::Propagation::Proceed
    }));

    state.backend_changed();
    window.present();
    search.grab_focus();
    dev_snapshot(&state);
    state
}

fn wire_search(state: &Rc<State>) {
    let search = &state.search;
    search.set_key_capture_widget(Some(&state.window));
    search.connect_search_changed(glib::clone!(#[weak] state, move |_| state.run_search()));
    search.connect_activate(glib::clone!(#[weak] state, move |_| {
        let first = state.hits.borrow().first().map(|h| h.path.clone());
        if let Some(path) = first {
            state.open(&path);
        }
    }));
    search.connect_stop_search(|s| s.set_text(""));
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(glib::clone!(#[weak] state, #[upgrade_or] glib::Propagation::Proceed, move |_, key, _, _| {
        if key == gdk::Key::Down && state.stack.visible_child_name().as_deref() == Some("results") {
            if let Some(row) = state.list.row_at_index(0) {
                state.list.select_row(Some(&row));
                row.grab_focus();
                return glib::Propagation::Stop;
            }
        }
        glib::Propagation::Proceed
    }));
    search.add_controller(keys);
}

fn wire_results(state: &Rc<State>) {
    let list = &state.list;
    list.connect_row_activated(glib::clone!(#[weak] state, move |_, row| {
        if let Some(path) = state.path_at(row.index()) {
            state.open(&path);
        }
    }));
    list.connect_row_selected(glib::clone!(#[weak] state, move |_, _| {
        if state.preview_split.shows_sidebar() {
            state.preview_selected();
        }
    }));
    list.connect_keynav_failed(glib::clone!(#[weak] state, #[upgrade_or] glib::Propagation::Proceed, move |_, direction| {
        if direction == gtk::DirectionType::Up {
            state.search.grab_focus();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    }));
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(#[weak] state, #[upgrade_or] glib::Propagation::Proceed, move |_, key, _, modifiers| {
        let Some(path) = state.list.selected_row().and_then(|r| state.path_at(r.index())) else {
            return glib::Propagation::Proceed;
        };
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::space => state.set_preview(!state.preview_split.shows_sidebar()),
            gdk::Key::Escape if state.preview_split.shows_sidebar() => state.set_preview(false),
            gdk::Key::Return | gdk::Key::KP_Enter if ctrl => state.show_in_folder(&path),
            gdk::Key::c if ctrl => state.copy_path(&path),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }));
    list.add_controller(keys);
}

/// Development aid: `FILEFIND_SNAPSHOT=out.png [FILEFIND_QUERY=text]` renders the window to a PNG.
fn dev_snapshot(state: &Rc<State>) {
    let Some(out) = std::env::var_os("FILEFIND_SNAPSHOT") else { return };
    if let Ok(query) = std::env::var("FILEFIND_QUERY") {
        state.search.set_text(&query);
    }
    let preview = std::env::var_os("FILEFIND_PREVIEW").is_some();
    let state = state.clone();
    glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
        if preview {
            if let Some(row) = state.list.row_at_index(0) {
                state.list.select_row(Some(&row));
            }
            state.set_preview(true);
        }
        let window = state.window.clone();
        glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
            let paintable = gtk::WidgetPaintable::new(Some(&window));
            let (w, h) = (window.width() as f64, window.height() as f64);
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(&snapshot, w, h);
            if let (Some(node), Some(renderer)) = (snapshot.to_node(), window.renderer()) {
                let texture = renderer.render_texture(node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
                if let Err(e) = texture.save_to_png(&out) {
                    log::error!("snapshot: {e}");
                }
            }
            window.close();
        });
    });
}

fn install_actions(state: &Rc<State>) {
    let s = state.clone();
    let path_action = |name: &str, f: fn(&State, &str)| {
        gio::ActionEntry::builder(name)
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(glib::clone!(#[weak] s, move |_: &adw::ApplicationWindow, _, p| {
                if let Some(path) = p.and_then(|p| p.get::<String>()) {
                    f(&s, &path);
                }
            }))
            .build()
    };
    let simple = |name: &str, f: fn(&Rc<State>)| {
        gio::ActionEntry::builder(name)
            .activate(glib::clone!(#[weak] s, move |_: &adw::ApplicationWindow, _, _| f(&s)))
            .build()
    };
    let actions = [
        path_action("open", |s, p| s.open(p)),
        path_action("show-in-folder", |s, p| s.show_in_folder(p)),
        path_action("copy-path", |s, p| s.copy_path(p)),
        path_action("preview", |s, p| s.preview_path(p)),
        path_action("remove-folder", |s, p| s.remove_folder(Path::new(p))),
        simple("open-previewed", |s| {
            if let Some(p) = s.preview.path() {
                s.open(&p);
            }
        }),
        simple("reveal-previewed", |s| {
            if let Some(p) = s.preview.path() {
                s.show_in_folder(&p);
            }
        }),
        simple("toggle-sidebar", |s| s.library_split.set_show_sidebar(!s.library_split.shows_sidebar())),
        simple("add-folder", |s| s.choose_folders()),
        simple("rebuild", |s| {
            s.backend.rebuild();
            s.toast(&tr("Rebuilding the index"));
        }),
        simple("settings", |s| crate::preferences::present(&s.window, &s.backend)),
        simple("show-failures", |s| crate::preferences::present_failures(&s.window, &s.backend)),
        simple("search-tips", |s| {
            let weak = Rc::downgrade(s);
            crate::help::present(&s.window, move |example| {
                if let Some(s) = weak.upgrade() {
                    s.search.set_text(example);
                    s.search.grab_focus();
                    s.search.set_position(-1);
                }
            });
        }),
        simple("focus-search", |s| {
            s.search.grab_focus();
            s.search.select_region(0, -1);
        }),
        simple("shortcuts", |s| show_shortcuts(&s.window)),
        simple("about", |s| {
            adw::AboutDialog::builder()
                .application_name(APP_NAME)
                .application_icon(APP_ID)
                .version(VERSION)
                .developer_name(tr("The Filefind Contributors"))
                .comments(tr("Find any file by what's inside it."))
                .license_type(gtk::License::Gpl30)
                .translator_credits(tr("translator-credits"))
                .build()
                .present(Some(&s.window));
        }),
    ];
    state.window.add_action_entries(actions);
}

fn show_shortcuts(window: &adw::ApplicationWindow) {
    let dialog = adw::PreferencesDialog::builder().title(tr("Keyboard Shortcuts")).content_width(420).search_enabled(false).build();
    let page = adw::PreferencesPage::new();
    let groups = [
        (tr("Search"), vec![
            (tr("Focus search"), "Ctrl+F".to_owned()),
            (tr("Open first result"), "Enter".to_owned()),
            (tr("Move to results"), "↓".to_owned()),
            (tr("Clear search"), "Esc".to_owned()),
        ]),
        (tr("Results"), vec![
            (tr("Open file"), tr("Enter or double-click")),
            (tr("Quick preview"), tr("Space")),
            (tr("Show in folder"), "Ctrl+Enter".to_owned()),
            (tr("Copy path"), "Ctrl+C".to_owned()),
            (tr("More actions"), tr("Right-click")),
        ]),
        (tr("Window"), vec![
            (tr("Show or hide the library"), "F9".to_owned()),
            (tr("Add folder"), "Ctrl+O".to_owned()),
            (tr("Settings"), "Ctrl+,".to_owned()),
            (tr("Close window"), "Ctrl+W".to_owned()),
            (tr("Quit"), "Ctrl+Q".to_owned()),
        ]),
    ];
    for (title, rows) in groups {
        let group = adw::PreferencesGroup::builder().title(title).build();
        for (label, keys) in rows {
            let row = adw::ActionRow::builder().title(label).build();
            let key = gtk::Label::new(Some(&keys));
            key.add_css_class("dim-label");
            row.add_suffix(&key);
            group.add(&row);
        }
        page.add(&group);
    }
    dialog.add(&page);
    dialog.present(Some(window));
}

impl State {
    pub fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    fn path_at(&self, index: i32) -> Option<String> {
        self.hits.borrow().get(usize::try_from(index).ok()?).map(|h| h.path.clone())
    }

    fn filters(&self) -> Filters {
        Filters {
            categories: self.filters.categories(),
            modified_since: self.filters.modified_since(),
            folder: self.sidebar.scope(),
        }
    }

    fn run_search(self: &Rc<Self>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        // Keep the raw text: a trailing space means the last word is complete.
        let request = self.backend.request(&self.search.text(), self.filters());
        if request.is_empty() {
            self.hits.borrow_mut().clear();
            self.list.remove_all();
            self.preview.clear();
            self.update_idle_page();
            return;
        }
        self.last_refresh.set(Some(std::time::Instant::now()));
        let state = self.clone();
        glib::spawn_future_local(async move {
            let results = state.backend.search(request).await;
            if generation == state.generation.get() {
                state.show_results(results);
            }
        });
    }

    fn show_results(&self, results: SearchResults) {
        let list = &self.list;
        list.remove_all();
        self.terms.replace(results.terms);
        if results.hits.is_empty() {
            self.stack.set_visible_child_name("empty");
            self.hits.borrow_mut().clear();
            self.preview.clear();
            return;
        }
        let accent = adw::StyleManager::default().accent_color_rgba();
        let accent = format!(
            "#{:02x}{:02x}{:02x}",
            (accent.red() * 255.0) as u8,
            (accent.green() * 255.0) as u8,
            (accent.blue() * 255.0) as u8
        );
        for hit in &results.hits {
            list.append(&crate::results::row(hit, &accent, &self.context_menu));
        }
        let shown = results.hits.len();
        self.count.set_label(&if results.total > shown {
            tr("Top {shown} of {total} results")
                .replace("{shown}", &shown.to_string())
                .replace("{total}", &fmt_count(results.total as u64))
        } else {
            ntr("{} result", "{} results", shown as u64).replace("{}", &shown.to_string())
        });
        self.hits.replace(results.hits);
        self.stack.set_visible_child_name("results");
        self.scroller.vadjustment().set_value(0.0);
        if self.preview_split.shows_sidebar() {
            list.select_row(list.row_at_index(0).as_ref());
        }
        log::debug!("search took {:?}", results.elapsed);
    }

    fn preview_selected(&self) {
        let index = self.list.selected_row().map(|r| r.index());
        let hit = index.and_then(|i| self.hits.borrow().get(usize::try_from(i).ok()?).cloned());
        match hit {
            Some(hit) => self.preview.show(&hit, self.terms.borrow().clone()),
            None => self.preview.clear(),
        }
    }

    fn preview_path(&self, path: &str) {
        let index = self.hits.borrow().iter().position(|h| h.path == path);
        if let Some(row) = index.and_then(|i| self.list.row_at_index(i as i32)) {
            self.list.select_row(Some(&row));
        }
        self.set_preview(true);
    }

    fn set_preview(&self, show: bool) {
        self.preview_wanted.set(show);
        self.preview_split.set_show_sidebar(show);
        if show {
            self.preview_selected();
        }
    }

    fn update_idle_page(&self) {
        let library = self.backend.library();
        if library.folders.is_empty() {
            self.stack.set_visible_child_name("welcome");
            return;
        }
        let folders = library.folders.len() as u64;
        let description = match self.backend.state() {
            IndexState::Ready(status) => ntr("{files} file in {folders}", "{files} files in {folders}", status.docs)
                .replace("{files}", &fmt_count(status.docs))
                .replace("{folders}", &ntr("{} folder", "{} folders", folders).replace("{}", &folders.to_string())),
            _ => tr("Reading your files. You can start searching right away."),
        };
        self.ready_page.set_description(Some(&description));
        self.stack.set_visible_child_name("ready");
    }

    fn update_status(&self) {
        let working = match self.backend.state() {
            IndexState::Working(p) if p.scanning && p.total > 0 => Some(
                ntr("Indexing… {} file so far", "Indexing… {} files so far", p.done as u64).replace("{}", &fmt_count(p.done as u64)),
            ),
            IndexState::Working(p) if p.total > 0 => Some(
                tr("Indexing {done} of {total} files…")
                    .replace("{done}", &fmt_count(p.done as u64))
                    .replace("{total}", &fmt_count(p.total as u64)),
            ),
            IndexState::Working(_) => Some(tr("Looking for files…")),
            _ => None,
        };
        let sidebar_visible = self.library_split.shows_sidebar();
        if let Some(label) = &working {
            self.status_label.set_label(label);
        }
        self.status_revealer.set_reveal_child(working.is_some() && !sidebar_visible);
    }

    fn backend_changed(self: &Rc<Self>) {
        self.sidebar.refresh();
        self.filters.sync_with_settings(&self.backend.settings());
        self.update_status();
        let working = matches!(self.backend.state(), IndexState::Working(_));
        let finished = self.was_working.replace(working) && !working;
        let searching = !self.backend.request(&self.search.text(), self.filters()).is_empty();
        if !searching {
            self.update_idle_page();
            return;
        }
        // Let new matches appear while the user is still typing, but never reshuffle a list
        // they are browsing.
        let stale = self.last_refresh.get().is_none_or(|t| t.elapsed().as_secs() >= 3);
        if self.search.has_focus() && (finished || (working && stale)) {
            self.run_search();
        }
    }

    pub fn open(&self, path: &str) {
        let toasts = self.toasts.clone();
        crate::launch::open(path, Some(self.window.upcast_ref()), move || toasts.add_toast(adw::Toast::new(&tr("Could not open the file"))));
    }

    pub fn show_in_folder(&self, path: &str) {
        let toasts = self.toasts.clone();
        crate::launch::show_in_folder(path, Some(self.window.upcast_ref()), move || {
            toasts.add_toast(adw::Toast::new(&tr("Could not show the folder")))
        });
    }

    fn copy_path(&self, path: &str) {
        self.window.clipboard().set_text(path);
        self.toast(&tr("Path copied"));
    }

    fn choose_folders(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder().title(tr("Add Folders to Library")).accept_label(tr("Add")).modal(true).build();
        let state = self.clone();
        dialog.select_multiple_folders(Some(&self.window), gio::Cancellable::NONE, move |res| {
            if let Ok(files) = res {
                let folders = files.iter::<gio::File>().filter_map(|f| f.ok()?.path()).collect();
                state.add_folders(folders);
            }
        });
    }

    fn add_folders(&self, folders: Vec<PathBuf>) {
        let outcomes: Vec<(String, AddOutcome)> = self.backend.update_library(|library| {
            folders.into_iter().map(|f| (display_name(&f), library.add(f))).collect()
        });
        for (name, outcome) in outcomes {
            let message = match outcome {
                AddOutcome::Added => continue,
                AddOutcome::AlreadyIncluded => tr("“{}” is already in your library"),
                AddOutcome::Merged(_) => tr("“{}” now includes folders that were added before"),
            };
            self.toast(&message.replace("{}", &name));
        }
    }

    fn remove_folder(&self, folder: &Path) {
        self.backend.update_library(|library| library.remove(folder));
        self.toast(&tr("Removed “{}” from your library").replace("{}", &display_name(folder)));
    }
}

pub fn display_path(path: &Path) -> String {
    let home = glib::home_dir();
    match path.strip_prefix(&home) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".to_owned(),
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

pub fn display_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}
