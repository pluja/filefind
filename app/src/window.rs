//! The main window: library sidebar, search field, live results and indexing status.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc};

use adw::prelude::*;
use filefind_core::{AddOutcome, Engine, Event, Extractor, Hit, Library, SearchResults, Segment, Service};
use gtk::{gdk, gio, glib, pango};
use serde::{Deserialize, Serialize};

use crate::i18n::{fmt_count, ntr, tr};
use crate::sidebar::Sidebar;
use crate::{APP_ID, APP_NAME, VERSION};

const MAX_RESULTS: usize = 100;

/// Window geometry and sidebar visibility, restored on the next launch.
#[derive(Serialize, Deserialize)]
struct WindowState {
    width: i32,
    height: i32,
    maximized: bool,
    sidebar: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        WindowState { width: 980, height: 680, maximized: false, sidebar: true }
    }
}

pub struct State {
    pub window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    split: adw::OverlaySplitView,
    sidebar: Sidebar,
    search: gtk::SearchEntry,
    stack: gtk::Stack,
    list: gtk::ListBox,
    count: gtk::Label,
    ready_page: adw::StatusPage,
    status_revealer: gtk::Revealer,
    status_label: gtk::Label,
    scroller: gtk::ScrolledWindow,
    context_menu: gtk::PopoverMenu,

    pub library: RefCell<Library>,
    config_dir: PathBuf,
    service: Option<Service>,
    search_tx: mpsc::Sender<(u64, String)>,
    generation: Cell<u64>,
    hits: RefCell<Vec<Hit>>,
    pub doc_count: Cell<u64>,
    indexing: Cell<bool>,
    last_refresh: Cell<Option<std::time::Instant>>,
}

pub fn build(app: &adw::Application) {
    let data_dir = glib::user_data_dir().join("filefind");
    let config_dir = glib::user_config_dir().join("filefind");
    let library = Library::load(&config_dir.join("library.json"));
    let saved: WindowState = std::fs::read(config_dir.join("window.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(APP_NAME)
        .default_width(saved.width)
        .default_height(saved.height)
        .maximized(saved.maximized)
        .width_request(360)
        .height_request(420)
        .build();

    // Header: sidebar toggle and menu. The search field sits in its own bar below.
    let header = adw::HeaderBar::new();
    header.set_show_title(false);
    let sidebar_toggle = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text(tr("Library"))
        .build();
    header.pack_start(&sidebar_toggle);
    let menu = gio::Menu::new();
    let section = gio::Menu::new();
    section.append(Some(&tr("_Add Folder…")), Some("win.add-folder"));
    section.append(Some(&tr("_Rebuild Index")), Some("win.rebuild"));
    menu.append_section(None, &section);
    let about = gio::Menu::new();
    about.append(Some(&tr("_Keyboard Shortcuts")), Some("win.shortcuts"));
    about.append(Some(&tr("_About Filefind")), Some("win.about"));
    menu.append_section(None, &about);
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text(tr("Main Menu"))
            .primary(true)
            .build(),
    );

    let search = gtk::SearchEntry::builder()
        .placeholder_text(tr("Search files and documents"))
        .hexpand(true)
        .search_delay(40)
        .build();
    search.add_css_class("hero-search");
    let search_bar = adw::Clamp::builder().maximum_size(720).tightening_threshold(560).child(&search).build();
    search_bar.add_css_class("search-bar");

    // Content pages.
    let welcome = adw::StatusPage::builder()
        .icon_name(APP_ID)
        .title(tr("Welcome to Filefind"))
        .description(tr("Add the folders you want to search. Filefind reads your documents — PDF, Word, LibreOffice, text and more — so you can find anything by what's inside."))
        .build();
    let add_button = gtk::Button::builder()
        .label(tr("Add Folder…"))
        .halign(gtk::Align::Center)
        .action_name("win.add-folder")
        .build();
    add_button.add_css_class("pill");
    add_button.add_css_class("suggested-action");
    welcome.set_child(Some(&add_button));

    let ready_page = adw::StatusPage::builder().icon_name("system-search-symbolic").title(tr("Search Your Files")).build();
    let empty = adw::StatusPage::builder()
        .icon_name("edit-find-symbolic")
        .title(tr("No Results Found"))
        .description(tr("Check the spelling or try different words."))
        .build();

    let list = gtk::ListBox::new();
    list.add_css_class("results");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_activate_on_single_click(true);
    let count = gtk::Label::builder().xalign(0.0).build();
    count.add_css_class("results-count");
    let results_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    results_box.append(&count);
    results_box.append(&list);
    let context_menu = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
    context_menu.set_has_arrow(false);
    context_menu.set_halign(gtk::Align::Start);
    context_menu.set_parent(&results_box);
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

    // Bottom status: shown only while indexing.
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

    // Library sidebar: docked on wide windows, overlaid on narrow ones.
    let sidebar = Sidebar::new();
    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar.widget)
        .content(&content)
        .show_sidebar(saved.sidebar)
        .min_sidebar_width(220.0)
        .max_sidebar_width(300.0)
        .sidebar_width_fraction(0.26)
        .build();
    split.bind_property("show-sidebar", &sidebar_toggle, "active").bidirectional().sync_create().build();
    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        640.0,
        adw::LengthUnit::Sp,
    ));
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    breakpoint.add_setter(&split, "show-sidebar", Some(&false.to_value()));
    window.add_breakpoint(breakpoint);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    window.set_content(Some(&toasts));

    // Search runs on its own thread; only the newest query is answered.
    let (search_tx, search_rx) = mpsc::channel::<(u64, String)>();
    let (results_tx, results_rx) = async_channel::unbounded::<(u64, SearchResults)>();
    let (event_tx, event_rx) = async_channel::unbounded::<Event>();

    let engine = match Engine::open(&data_dir.join("index")) {
        Ok(engine) => Some(Arc::new(engine)),
        Err(e) => {
            log::error!("cannot open index: {e}");
            None
        }
    };
    let service = engine.as_ref().map(|engine| {
        let helper = std::env::current_exe().ok();
        Service::start(engine.clone(), Extractor { helper }, move |event| {
            let _ = event_tx.send_blocking(event);
        })
    });
    if let Some(engine) = engine.clone() {
        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                while let Ok(mut request) = search_rx.recv() {
                    while let Ok(newer) = search_rx.try_recv() {
                        request = newer;
                    }
                    let results = engine.search(&request.1, MAX_RESULTS);
                    if results_tx.send_blocking((request.0, results)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn search thread");
    }

    let state = Rc::new(State {
        window: window.clone(),
        toasts,
        split: split.clone(),
        sidebar,
        search: search.clone(),
        stack,
        list: list.clone(),
        count,
        ready_page,
        status_revealer,
        status_label,
        scroller,
        context_menu,
        library: RefCell::new(library),
        config_dir,
        service,
        search_tx,
        generation: Cell::new(0),
        hits: RefCell::new(Vec::new()),
        doc_count: Cell::new(engine.as_ref().map_or(0, |e| e.num_docs())),
        indexing: Cell::new(false),
        last_refresh: Cell::new(None),
    });

    if engine.is_none() {
        state.toast(&tr("The search index could not be opened. Try Rebuild Index from the menu."));
    }

    // Wire up search input.
    search.set_key_capture_widget(Some(&window));
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

    list.connect_row_activated(glib::clone!(#[weak] state, move |_, row| {
        if let Some(path) = state.path_at(row.index()) {
            state.open(&path);
        }
    }));
    list.connect_keynav_failed(glib::clone!(#[weak] state, #[upgrade_or] glib::Propagation::Proceed, move |_, direction| {
        if direction == gtk::DirectionType::Up {
            state.search.grab_focus();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    }));
    let list_keys = gtk::EventControllerKey::new();
    list_keys.connect_key_pressed(glib::clone!(#[weak] state, #[upgrade_or] glib::Propagation::Proceed, move |_, key, _, modifiers| {
        let Some(path) = state.list.selected_row().and_then(|r| state.path_at(r.index())) else {
            return glib::Propagation::Proceed;
        };
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::Return | gdk::Key::KP_Enter if ctrl => state.show_in_folder(&path),
            gdk::Key::c if ctrl => state.copy_path(&path),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }));
    list.add_controller(list_keys);

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

    install_actions(&state);

    glib::spawn_future_local(glib::clone!(#[weak] state, async move {
        while let Ok((generation, results)) = results_rx.recv().await {
            if generation == state.generation.get() {
                state.show_results(results);
            }
        }
    }));
    glib::spawn_future_local(glib::clone!(#[weak] state, async move {
        while let Ok(event) = event_rx.recv().await {
            state.handle_event(event);
        }
    }));

    // Remember size and sidebar. This handler also keeps the State alive as long as the window.
    window.connect_close_request(glib::clone!(#[strong] state, move |window| {
        let (width, height) = window.default_size();
        let saved = WindowState {
            width,
            height,
            maximized: window.is_maximized(),
            sidebar: state.split.is_collapsed() || state.split.shows_sidebar(),
        };
        if let Ok(json) = serde_json::to_vec_pretty(&saved) {
            let _ = std::fs::create_dir_all(&state.config_dir);
            let _ = std::fs::write(state.config_dir.join("window.json"), json);
        }
        glib::Propagation::Proceed
    }));

    if let Some(service) = &state.service {
        service.set_folders(state.library.borrow().folders.clone());
    }
    state.sidebar.refresh(&state);
    state.update_idle_page();
    window.present();
    search.grab_focus();
    dev_snapshot(&state);
}

/// Development aid: `FILEFIND_SNAPSHOT=out.png [FILEFIND_QUERY=text]` renders the window to a PNG.
fn dev_snapshot(state: &Rc<State>) {
    let Some(out) = std::env::var_os("FILEFIND_SNAPSHOT") else { return };
    if let Ok(query) = std::env::var("FILEFIND_QUERY") {
        state.search.set_text(&query);
    }
    let window = state.window.clone();
    glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
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
}

fn install_actions(state: &Rc<State>) {
    let window = &state.window;
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
    let open = path_action("open", State::open);
    let show = path_action("show-in-folder", State::show_in_folder);
    let copy = path_action("copy-path", State::copy_path);
    let toggle_sidebar = gio::ActionEntry::builder("toggle-sidebar").activate(glib::clone!(#[weak] s, move |_: &adw::ApplicationWindow, _, _| {
        s.split.set_show_sidebar(!s.split.shows_sidebar());
    })).build();
    let add_folder = gio::ActionEntry::builder("add-folder").activate(glib::clone!(#[weak] s, move |_: &adw::ApplicationWindow, _, _| {
        s.choose_folders();
    })).build();
    let rebuild = gio::ActionEntry::builder("rebuild").activate(glib::clone!(#[weak] s, move |_: &adw::ApplicationWindow, _, _| {
        s.rebuild();
    })).build();
    let focus = gio::ActionEntry::builder("focus-search").activate(glib::clone!(#[weak] s, move |_: &adw::ApplicationWindow, _, _| {
        s.search.grab_focus();
        s.search.select_region(0, -1);
    })).build();
    let shortcuts = gio::ActionEntry::builder("shortcuts").activate(glib::clone!(#[weak] s, move |_: &adw::ApplicationWindow, _, _| {
        show_shortcuts(&s.window);
    })).build();
    let about = gio::ActionEntry::builder("about").activate(glib::clone!(#[weak] s, move |_: &adw::ApplicationWindow, _, _| {
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
    })).build();
    window.add_action_entries([open, show, copy, toggle_sidebar, add_folder, rebuild, focus, shortcuts, about]);
}

fn show_shortcuts(window: &adw::ApplicationWindow) {
    let dialog = adw::PreferencesDialog::builder()
        .title(tr("Keyboard Shortcuts"))
        .content_width(420)
        .search_enabled(false)
        .build();
    let page = adw::PreferencesPage::new();
    let groups = [
        (tr("Search"), vec![
            (tr("Focus search"), "Ctrl+F".to_owned()),
            (tr("Open first result"), "Enter".to_owned()),
            (tr("Move to results"), "↓".to_owned()),
            (tr("Clear search"), "Esc".to_owned()),
        ]),
        (tr("Results"), vec![
            (tr("Open file"), "Enter".to_owned()),
            (tr("Show in folder"), "Ctrl+Enter".to_owned()),
            (tr("Copy path"), "Ctrl+C".to_owned()),
            (tr("More actions"), tr("Right-click")),
        ]),
        (tr("Window"), vec![
            (tr("Show or hide the library"), "F9".to_owned()),
            (tr("Add folder"), "Ctrl+O".to_owned()),
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

    fn run_search(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        if self.search.text().trim().is_empty() {
            self.hits.borrow_mut().clear();
            self.list.remove_all();
            self.update_idle_page();
            return;
        }
        self.last_refresh.set(Some(std::time::Instant::now()));
        // Keep the raw text: a trailing space means the last word is complete.
        let _ = self.search_tx.send((generation, self.search.text().to_string()));
    }

    fn show_results(&self, results: SearchResults) {
        let list = &self.list;
        list.remove_all();
        if results.hits.is_empty() {
            self.stack.set_visible_child_name("empty");
            self.hits.borrow_mut().clear();
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
            list.append(&self.result_row(hit, &accent));
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
        log::debug!("search took {:?}", results.elapsed);
    }

    fn result_row(&self, hit: &Hit, accent: &str) -> gtk::ListBoxRow {
        let path = Path::new(&hit.path);
        let (content_type, _) = gio::content_type_guess(Some(path), None::<&[u8]>);
        let icon = gtk::Image::from_gicon(&gio::content_type_get_icon(&content_type));
        icon.set_pixel_size(32);
        icon.set_valign(gtk::Align::Start);
        icon.set_margin_top(2);

        let name = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(pango::EllipsizeMode::Middle).build();
        name.set_markup(&markup(&hit.name, &format!("<span foreground=\"{accent}\">"), "</span>", false));
        name.add_css_class("result-title");
        let date = gtk::Label::new(Some(&fmt_date(hit.mtime)));
        date.add_css_class("result-date");
        date.set_tooltip_text(Some(&glib::format_size(hit.size)));
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        top.append(&name);
        top.append(&date);

        let folder = path.parent().map(display_path).unwrap_or_default();
        let location = gtk::Label::builder().label(&folder).xalign(0.0).ellipsize(pango::EllipsizeMode::Middle).build();
        location.add_css_class("result-path");

        let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
        text.set_hexpand(true);
        text.append(&top);
        text.append(&location);
        if !hit.snippet.is_empty() {
            let snippet = gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(pango::WrapMode::WordChar)
                .lines(2)
                .ellipsize(pango::EllipsizeMode::End)
                .width_chars(20)
                .build();
            snippet.set_markup(&markup(&hit.snippet, "<span weight=\"bold\" alpha=\"100%\">", "</span>", true));
            snippet.add_css_class("result-snippet");
            text.append(&snippet);
        }

        let reveal = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text(tr("Show in Folder"))
            .valign(gtk::Align::Center)
            .action_name("win.show-in-folder")
            .action_target(&hit.path.to_variant())
            .build();
        reveal.add_css_class("flat");
        reveal.add_css_class("circular");
        reveal.add_css_class("result-action");

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.append(&icon);
        content.append(&text);
        content.append(&reveal);
        let row = gtk::ListBoxRow::builder().child(&content).build();
        row.set_tooltip_text(Some(&hit.path));

        // Right-click / long-press menu, shared by all rows.
        let click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        let menu = self.context_menu.clone();
        let target = hit.path.clone();
        click.connect_pressed(glib::clone!(#[weak] row, #[strong] menu, #[strong] target, move |_, _, x, y| {
            show_context_menu(&menu, &row, x, y, &target);
        }));
        row.add_controller(click);
        let long_press = gtk::GestureLongPress::new();
        long_press.connect_pressed(glib::clone!(#[weak] row, move |_, x, y| {
            show_context_menu(&menu, &row, x, y, &target);
        }));
        row.add_controller(long_press);
        row
    }

    fn update_idle_page(&self) {
        if !self.search.text().trim().is_empty() {
            return;
        }
        let library = self.library.borrow();
        if library.folders.is_empty() {
            self.stack.set_visible_child_name("welcome");
            return;
        }
        let folders = library.folders.len() as u64;
        let docs = self.doc_count.get();
        let description = if self.indexing.get() && docs == 0 {
            tr("Reading your files. You can start searching right away.")
        } else {
            ntr("{files} file in {folders}", "{files} files in {folders}", docs)
                .replace("{files}", &fmt_count(docs))
                .replace("{folders}", &ntr("{} folder", "{} folders", folders).replace("{}", &folders.to_string()))
        };
        self.ready_page.set_description(Some(&description));
        self.stack.set_visible_child_name("ready");
    }

    fn handle_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::Indexing { done, total } => {
                self.indexing.set(true);
                self.status_label.set_label(
                    &tr("Indexing {done} of {total} files…")
                        .replace("{done}", &fmt_count(done as u64))
                        .replace("{total}", &fmt_count(total as u64)),
                );
                self.status_revealer.set_reveal_child(true);
                // Let new matches trickle in while the user is still on the search field,
                // but never reshuffle a list they are browsing.
                let stale = self.last_refresh.get().is_none_or(|t| t.elapsed().as_secs() >= 3);
                if stale && self.search.has_focus() && !self.search.text().trim().is_empty() {
                    self.run_search();
                }
            }
            Event::Idle { docs } => {
                let was_indexing = self.indexing.replace(false);
                self.doc_count.set(docs);
                self.status_revealer.set_reveal_child(false);
                if was_indexing && self.search.has_focus() && !self.search.text().trim().is_empty() {
                    self.run_search();
                }
                self.update_idle_page();
            }
            Event::Error(message) => {
                log::error!("{message}");
                self.indexing.set(false);
                self.status_revealer.set_reveal_child(false);
                self.toast(&tr("Could not update the search index"));
            }
        }
        self.sidebar.refresh(self);
    }

    fn launcher(path: &str) -> gtk::FileLauncher {
        gtk::FileLauncher::new(Some(&gio::File::for_path(path)))
    }

    pub fn open(&self, path: &str) {
        let toasts = self.toasts.clone();
        Self::launcher(path).launch(Some(&self.window), gio::Cancellable::NONE, move |res| {
            if let Err(e) = res {
                if !e.matches(gtk::DialogError::Dismissed) {
                    toasts.add_toast(adw::Toast::new(&tr("Could not open the file")));
                    log::warn!("open: {e}");
                }
            }
        });
    }

    pub fn show_in_folder(&self, path: &str) {
        let toasts = self.toasts.clone();
        Self::launcher(path).open_containing_folder(Some(&self.window), gio::Cancellable::NONE, move |res| {
            if let Err(e) = res {
                if !e.matches(gtk::DialogError::Dismissed) {
                    toasts.add_toast(adw::Toast::new(&tr("Could not show the folder")));
                    log::warn!("show in folder: {e}");
                }
            }
        });
    }

    fn copy_path(&self, path: &str) {
        self.window.clipboard().set_text(path);
        self.toast(&tr("Path copied"));
    }

    pub fn choose_folders(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title(tr("Add Folders to Library"))
            .accept_label(tr("Add"))
            .modal(true)
            .build();
        let state = self.clone();
        dialog.select_multiple_folders(Some(&self.window), gio::Cancellable::NONE, move |res| {
            if let Ok(files) = res {
                let folders = files.iter::<gio::File>().filter_map(|f| f.ok()?.path()).collect();
                state.add_folders(folders);
            }
        });
    }

    pub fn add_folders(self: &Rc<Self>, folders: Vec<PathBuf>) {
        let mut messages = Vec::new();
        {
            let mut library = self.library.borrow_mut();
            for folder in folders {
                let name = display_name(&folder);
                match library.add(folder) {
                    AddOutcome::AlreadyIncluded => messages.push(tr("“{}” is already in your library").replace("{}", &name)),
                    AddOutcome::Merged(_) => messages.push(tr("“{}” now includes folders that were added before").replace("{}", &name)),
                    AddOutcome::Added => {}
                }
            }
        }
        for message in messages {
            self.toast(&message);
        }
        self.library_updated();
    }

    pub fn remove_folder(self: &Rc<Self>, folder: &Path) {
        self.library.borrow_mut().remove(folder);
        self.library_updated();
        self.toast(&tr("Removed “{}” from your library").replace("{}", &display_name(folder)));
    }

    fn library_updated(self: &Rc<Self>) {
        let library = self.library.borrow().clone();
        if let Err(e) = library.save(&self.config_dir.join("library.json")) {
            log::error!("saving library: {e}");
            self.toast(&tr("Could not save your library"));
        }
        if let Some(service) = &self.service {
            service.set_folders(library.folders);
        }
        self.update_idle_page();
        self.sidebar.refresh(self);
    }

    pub fn rebuild(&self) {
        match &self.service {
            Some(service) => {
                service.rebuild();
                self.toast(&tr("Rebuilding the index"));
            }
            None => self.toast(&tr("Restart Filefind to rebuild the index")),
        }
    }

    pub fn is_indexing(&self) -> bool {
        self.indexing.get()
    }
}

fn show_context_menu(popover: &gtk::PopoverMenu, row: &gtk::ListBoxRow, x: f64, y: f64, path: &str) {
    let Some(anchor) = popover.parent() else { return };
    if let Some(list) = row.parent().and_downcast::<gtk::ListBox>() {
        list.select_row(Some(row));
    }
    let menu = gio::Menu::new();
    for (label, action) in [(tr("Open"), "win.open"), (tr("Show in Folder"), "win.show-in-folder"), (tr("Copy Path"), "win.copy-path")] {
        let item = gio::MenuItem::new(Some(&label), None);
        item.set_action_and_target_value(Some(action), Some(&path.to_variant()));
        menu.append_item(&item);
    }
    popover.set_menu_model(Some(&menu));
    let origin = gtk::graphene::Point::new(x as f32, y as f32);
    let point = row.compute_point(&anchor, &origin).unwrap_or(origin);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(point.x() as i32, point.y() as i32, 1, 1)));
    popover.popup();
}

/// Converts highlighted segments into Pango markup.
fn markup(segments: &[Segment], open: &str, close: &str, dim_plain: bool) -> String {
    let mut out = String::new();
    for (text, highlighted) in segments {
        let escaped = glib::markup_escape_text(text);
        if *highlighted {
            out.push_str(open);
            out.push_str(&escaped);
            out.push_str(close);
        } else if dim_plain {
            out.push_str("<span alpha=\"70%\">");
            out.push_str(&escaped);
            out.push_str("</span>");
        } else {
            out.push_str(&escaped);
        }
    }
    out
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

fn fmt_date(mtime: u64) -> String {
    let Ok(date) = glib::DateTime::from_unix_local(mtime as i64) else { return String::new() };
    let Ok(now) = glib::DateTime::now_local() else { return String::new() };
    let format = if date.ymd() == now.ymd() {
        "%R"
    } else if date.year() == now.year() {
        "%e %b"
    } else {
        "%e %b %Y"
    };
    date.format(format).map(|s| s.trim().to_owned()).unwrap_or_default()
}
