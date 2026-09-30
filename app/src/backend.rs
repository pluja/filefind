//! App-wide state shared by the window and the system search providers: the index, the
//! indexer, the library and the settings.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use filefind_core::{Engine, Event, Extractor, FailedFile, Filters, Library, Progress, SearchRequest, SearchResults, Service, Status};
use gtk::{gio, glib};

use crate::i18n::tr;
use crate::settings::Settings;

/// Name of the folder created when the user picks a custom index location.
const INDEX_FOLDER: &str = "Filefind Index";

#[derive(Clone, Debug, Default, PartialEq)]
pub enum IndexState {
    /// Indexing hasn't started in this session.
    #[default]
    Stopped,
    Working(Progress),
    Ready(Status),
    /// The index could not be opened or written.
    Broken,
}

type Listener = Rc<dyn Fn()>;

pub struct Backend {
    app: glib::WeakRef<adw::Application>,
    config_dir: PathBuf,
    default_index_dir: PathBuf,
    engine: RefCell<Option<Arc<Engine>>>,
    service: RefCell<Option<Service>>,
    events: async_channel::Sender<Event>,
    library: RefCell<Library>,
    settings: RefCell<Settings>,
    state: RefCell<IndexState>,
    /// Extracts text for previews, independently of indexing.
    previews: Arc<Extractor>,
    background_hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    /// Signals when a stopping indexer has released the index.
    stopping: RefCell<Option<async_channel::Receiver<()>>>,
    moving: Cell<bool>,
    listeners: RefCell<Vec<(u64, Listener)>>,
    next_listener: Cell<u64>,
}

fn extractor(interactive: bool) -> Extractor {
    match std::env::current_exe() {
        Ok(exe) if interactive => Extractor::with_interactive_helper(exe),
        Ok(exe) => Extractor::with_helper(exe),
        Err(_) => Extractor::in_process(),
    }
}

impl Backend {
    pub fn new(app: &adw::Application) -> Rc<Backend> {
        let config_dir = glib::user_config_dir().join("filefind");
        let default_index_dir = glib::user_data_dir().join("filefind").join("index");
        let settings: Settings = Settings::load(&config_dir.join("settings.json"));
        let library = Library::load(&config_dir.join("library.json"));
        let (events, event_rx) = async_channel::unbounded();
        let backend = Rc::new(Backend {
            app: app.downgrade(),
            config_dir,
            default_index_dir,
            engine: RefCell::new(None),
            service: RefCell::new(None),
            events,
            library: RefCell::new(library),
            settings: RefCell::new(settings),
            state: RefCell::new(IndexState::Stopped),
            previews: Arc::new(extractor(true)),
            background_hold: RefCell::new(None),
            stopping: RefCell::new(None),
            moving: Cell::new(false),
            listeners: RefCell::new(Vec::new()),
            next_listener: Cell::new(0),
        });
        backend.open_engine();

        let weak = Rc::downgrade(&backend);
        glib::spawn_future_local(async move {
            while let Ok(event) = event_rx.recv().await {
                let Some(backend) = weak.upgrade() else { break };
                backend.handle(event);
            }
        });
        if backend.settings().background {
            backend.hold_for_background(true);
        }
        backend
    }

    fn open_engine(&self) {
        let dir = self.index_dir();
        match Engine::open(&dir) {
            Ok(engine) => {
                self.engine.replace(Some(Arc::new(engine)));
            }
            Err(e) => {
                log::error!("cannot open index at {}: {e}", dir.display());
                self.engine.replace(None);
                self.state.replace(IndexState::Broken);
            }
        }
    }

    pub fn index_dir(&self) -> PathBuf {
        self.settings.borrow().index_dir.clone().unwrap_or_else(|| self.default_index_dir.clone())
    }

    pub fn default_index_dir(&self) -> &Path {
        &self.default_index_dir
    }

    pub fn library(&self) -> Library {
        self.library.borrow().clone()
    }

    pub fn settings(&self) -> Settings {
        self.settings.borrow().clone()
    }

    pub fn state(&self) -> IndexState {
        self.state.borrow().clone()
    }

    /// Calls `f` whenever the library, settings or index state change. Returns an id for
    /// [`Backend::unsubscribe`].
    pub fn subscribe(&self, f: impl Fn() + 'static) -> u64 {
        let id = self.next_listener.get();
        self.next_listener.set(id + 1);
        self.listeners.borrow_mut().push((id, Rc::new(f)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.listeners.borrow_mut().retain(|(i, _)| *i != id);
    }

    fn notify(&self) {
        // Clone first: a listener may subscribe or unsubscribe.
        let listeners: Vec<Listener> = self.listeners.borrow().iter().map(|(_, f)| f.clone()).collect();
        for f in listeners {
            f();
        }
    }

    fn handle(&self, event: Event) {
        let state = match event {
            Event::Progress(p) => IndexState::Working(p),
            Event::Idle(status) => IndexState::Ready(status),
            Event::Error(message) => {
                log::error!("{message}");
                IndexState::Broken
            }
        };
        self.state.replace(state);
        self.notify();
    }

    /// Starts keeping the index up to date, if it isn't already.
    pub fn start_indexing(self: &Rc<Self>) {
        if self.service.borrow().is_some() || self.moving.get() {
            return;
        }
        // A previous indexer may still hold the index while saving; start once it's done.
        if let Some(stopped) = self.stopping.borrow().clone() {
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                let _ = stopped.recv().await;
                if let Some(backend) = weak.upgrade() {
                    backend.stopping.replace(None);
                    backend.start_indexing();
                }
            });
            return;
        }
        let Some(engine) = self.engine.borrow().clone() else { return };
        let events = self.events.clone();
        let service = Service::start(engine, extractor(false), move |event| {
            let _ = events.send_blocking(event);
        });
        service.set_library(self.library.borrow().folders.clone(), self.settings.borrow().index_options());
        self.service.replace(Some(service));
        self.state.replace(IndexState::Working(Progress { scanning: true, ..Default::default() }));
        self.notify();
    }

    /// Stops indexing in the background; the app stays alive until progress is saved.
    pub fn stop_indexing(&self) {
        let Some(service) = self.service.take() else { return };
        let hold = self.app.upgrade().map(|app| app.hold());
        let (done, stopped) = async_channel::bounded(1);
        self.stopping.replace(Some(stopped));
        glib::spawn_future_local(async move {
            let _ = gio::spawn_blocking(move || service.shutdown()).await;
            // Closing the channel wakes every waiter.
            drop(done);
            drop(hold);
        });
        self.state.replace(IndexState::Stopped);
    }

    /// Waits until a stopping indexer has released the index.
    async fn stopped(&self) {
        let stopping = self.stopping.borrow().clone();
        if let Some(stopped) = stopping {
            let _ = stopped.recv().await;
        }
        self.stopping.replace(None);
    }

    pub fn is_moving(&self) -> bool {
        self.moving.get()
    }

    /// A request for `query` using the user's search preferences.
    pub fn request(&self, query: &str, filters: Filters) -> SearchRequest {
        let settings = self.settings.borrow();
        SearchRequest {
            query: query.to_owned(),
            filters,
            sort: settings.sort.to_core(),
            limit: settings.max_results,
            word_forms: settings.word_forms,
        }
    }

    /// Runs a search off the main thread.
    pub async fn search(&self, request: SearchRequest) -> SearchResults {
        let Some(engine) = self.engine.borrow().clone() else { return SearchResults::default() };
        gio::spawn_blocking(move || engine.search(&request)).await.unwrap_or_default()
    }

    /// Extracts the full text of a file for the preview, off the main thread.
    pub async fn read_text(&self, path: PathBuf) -> Option<String> {
        let extractor = self.previews.clone();
        gio::spawn_blocking(move || {
            let kind = filefind_core::extract::kind_for(&path)?;
            let text = extractor.extract(&path, kind).ok();
            // This worker thread may exit, and helpers die with the thread that started them.
            extractor.release_idle();
            text
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn failed_files(&self) -> Vec<FailedFile> {
        let Some(engine) = self.engine.borrow().clone() else { return Vec::new() };
        gio::spawn_blocking(move || engine.failed_files(1000)).await.unwrap_or_default()
    }

    pub async fn index_size(&self) -> u64 {
        let dir = self.index_dir();
        gio::spawn_blocking(move || dir_size(&dir)).await.unwrap_or(0)
    }

    /// Changes the library, saves it and reindexes accordingly. Returns what `f` returns.
    pub fn update_library<R>(&self, f: impl FnOnce(&mut Library) -> R) -> R {
        let result = f(&mut self.library.borrow_mut());
        if let Err(e) = self.library.borrow().save(&self.config_dir.join("library.json")) {
            log::error!("saving library: {e}");
        }
        self.push_library();
        self.notify();
        result
    }

    /// Tells the indexer what to index.
    fn push_library(&self) {
        if let Some(service) = self.service.borrow().as_ref() {
            service.set_library(self.library.borrow().folders.clone(), self.settings.borrow().index_options());
        }
    }

    pub fn update_settings(self: &Rc<Self>, f: impl FnOnce(&mut Settings)) {
        let before = self.settings();
        f(&mut self.settings.borrow_mut());
        let after = self.settings();
        if after == before {
            return;
        }
        self.save_settings();
        if after.index_options() != before.index_options() {
            self.push_library();
        }
        if after.background != before.background {
            self.hold_for_background(after.background);
            request_background(after.background);
            if after.background {
                self.start_indexing();
            }
        }
        self.notify();
    }

    fn save_settings(&self) {
        if let Err(e) = self.settings.borrow().save(&self.config_dir.join("settings.json")) {
            log::error!("saving settings: {e}");
        }
    }

    /// Keeps the app running without windows while background indexing is on.
    pub fn hold_for_background(&self, hold: bool) {
        let guard = if hold { self.app.upgrade().map(|app| app.hold()) } else { None };
        self.background_hold.replace(guard);
    }

    pub fn rebuild(&self) {
        if let Some(service) = self.service.borrow().as_ref() {
            service.rebuild();
        }
    }

    /// Moves the index into a new folder inside `parent` (`None`: back to the default
    /// location). The old copy is only removed once the new one has been verified.
    pub async fn move_index(self: Rc<Self>, parent: Option<PathBuf>) -> Result<(), String> {
        if self.moving.replace(true) {
            return Err(tr("The index is already being moved."));
        }
        self.notify();
        let result = self.clone().move_index_inner(parent).await;
        self.moving.set(false);
        self.open_engine();
        if self.settings().background || self.app.upgrade().is_some_and(|app| !app.windows().is_empty()) {
            self.start_indexing();
        }
        self.notify();
        result.map_err(|e| tr("Could not move the index: {}").replace("{}", &e))
    }

    async fn move_index_inner(self: Rc<Self>, parent: Option<PathBuf>) -> Result<(), String> {
        let from = self.index_dir();
        let target = parent.map(|p| p.join(INDEX_FOLDER));
        let to = target.clone().unwrap_or_else(|| self.default_index_dir.clone());
        let resolved = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
        let (from_real, to_real) = (resolved(&from), resolved(to.parent().unwrap_or(&to)).join(to.file_name().unwrap_or_default()));
        if to_real == from_real {
            return Ok(());
        }
        if to_real.starts_with(&from_real) || from_real.starts_with(&to_real) {
            return Err(tr("Choose a folder outside the current index."));
        }
        // The index folder is owned by Filefind and may be deleted: never reuse an existing one.
        if std::fs::read_dir(&to).is_ok_and(|mut entries| entries.next().is_some()) {
            return Err(tr("There is already a folder named “{}” there.").replace("{}", INDEX_FOLDER));
        }
        if std::fs::create_dir_all(&to).and_then(|_| std::fs::write(to.join(".write-test"), b"")).is_err() {
            let _ = std::fs::remove_dir(&to);
            return Err(tr("Filefind can't write to that folder."));
        }
        let _ = std::fs::remove_file(to.join(".write-test"));

        if let Some(service) = self.service.take() {
            let _ = gio::spawn_blocking(move || service.shutdown()).await;
        }
        self.stopped().await;
        let expected = self.engine.borrow().as_ref().map_or(0, |e| e.num_docs());
        self.engine.replace(None);

        let copy = {
            let (from, to) = (from.clone(), to.clone());
            gio::spawn_blocking(move || -> Result<(), String> {
                copy_dir(&from, &to).map_err(|e| e.to_string())?;
                let engine = Engine::open(&to).map_err(|e| e.to_string())?;
                if engine.num_docs() != expected {
                    return Err("the copied index is incomplete".into());
                }
                Ok(())
            })
            .await
            .unwrap_or_else(|_| Err("copy failed".into()))
        };
        match copy {
            Ok(()) => {
                self.settings.borrow_mut().index_dir = target;
                self.save_settings();
                gio::spawn_blocking(move || std::fs::remove_dir_all(from));
                Ok(())
            }
            Err(e) => {
                // Only the new folder is removed; it was created empty above.
                let _ = std::fs::remove_dir_all(&to);
                Err(e)
            }
        }
    }
}

/// Asks the desktop to let Filefind run in the background and start at login (Flatpak).
fn request_background(enable: bool) {
    let options = glib::VariantDict::new(None);
    options.insert_value("reason", &tr("Keep your search index up to date").to_variant());
    options.insert_value("autostart", &enable.to_variant());
    options.insert_value("commandline", &vec!["filefind".to_owned(), "--background".to_owned()].to_variant());
    let args = (String::new(), options.end()).to_variant();
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, move |bus| {
        let Ok(bus) = bus else { return };
        bus.call(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Background",
            "RequestBackground",
            Some(&args),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            |res| {
                if let Err(e) = res {
                    log::info!("background portal unavailable: {e}");
                }
            },
        );
    });
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(Result::ok).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0)
}
