//! Background indexing: keeps the index in sync with the library folders.
//!
//! A worker thread owns the index writer. A full sync is a pipeline: a scanner thread
//! walks the folders and queues changed files, extraction threads turn them into
//! documents, and the worker adds them, reports progress and commits periodically.
//! Afterwards file changes are followed through inotify, with a periodic rescan as a
//! safety net. All of it runs at idle CPU and I/O priority.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, UNIX_EPOCH};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use tantivy::{IndexWriter, TantivyDocument};

use crate::engine::{Content, Engine, FileMeta};
use crate::extract::{self, Extractor, Kind};
use crate::filter::IndexOptions;
use crate::library::Folder;

const COMMIT_EVERY: Duration = Duration::from_secs(10);
const MIN_COMMIT_GAP: Duration = Duration::from_secs(5);
const CHANGE_CHUNK: usize = 64;
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
const DEBOUNCE: Duration = Duration::from_millis(1500);
const RESCAN_EVERY: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// The folders are still being scanned, so `total` may grow.
    pub scanning: bool,
    pub scanned: usize,
    /// Files that need (re)indexing, found so far.
    pub total: usize,
    pub done: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub docs: u64,
    pub failed: u64,
    /// Indexed files per library folder, in library order.
    pub folders: Vec<u64>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Progress(Progress),
    /// The index is up to date.
    Idle(Status),
    Error(String),
}

enum Cmd {
    Library(Vec<Folder>, IndexOptions),
    Rescan,
    Rebuild,
    Changed(Vec<PathBuf>),
    Stop,
}

impl Cmd {
    /// Commands that make a running full sync pointless.
    fn supersedes_sync(&self) -> bool {
        matches!(self, Cmd::Library(..) | Cmd::Rebuild | Cmd::Rescan | Cmd::Stop)
    }
}

pub struct Service {
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Service {
    pub fn start(engine: Arc<Engine>, extractor: Extractor, on_event: impl Fn(Event) + Send + Sync + 'static) -> Service {
        let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(on_event);
        let (tx, rx) = mpsc::channel();
        let watch_tx = tx.clone();
        let thread = std::thread::Builder::new()
            .name("indexer".into())
            .spawn(move || {
                // Before creating the writer, so its threads inherit the priority.
                crate::priority::lower_current_thread();
                let writer = match engine.writer() {
                    Ok(w) => w,
                    Err(e) => {
                        on_event(Event::Error(format!("Could not open the index: {e}")));
                        return;
                    }
                };
                let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    use notify::event::ModifyKind;
                    let Ok(ev) = res else { return };
                    let relevant = matches!(
                        ev.kind,
                        EventKind::Create(_)
                            | EventKind::Remove(_)
                            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any)
                    );
                    if relevant && !ev.paths.is_empty() {
                        let _ = watch_tx.send(Cmd::Changed(ev.paths));
                    }
                })
                .map_err(|e| log::warn!("file watching unavailable: {e}"))
                .ok();
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(extraction_threads())
                    .thread_name(|i| format!("extract-{i}"))
                    .start_handler(|_| crate::priority::lower_current_thread())
                    .build()
                    .expect("thread pool");
                let worker = Worker {
                    engine,
                    extraction: Arc::new(Extraction { extractor, pool }),
                    writer,
                    rx,
                    watcher,
                    watched: HashSet::new(),
                    watch_full: false,
                    roots: Vec::new(),
                    options: IndexOptions::default(),
                    pending: VecDeque::new(),
                    last_commit: Instant::now(),
                    on_event: on_event.clone(),
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run()));
                if result.is_err() {
                    on_event(Event::Error("The indexer stopped unexpectedly".into()));
                }
            })
            .expect("spawn indexer");
        Service { tx, thread: Some(thread) }
    }

    /// Sets what to index and reconciles the index with it.
    pub fn set_library(&self, folders: Vec<Folder>, options: IndexOptions) {
        let _ = self.tx.send(Cmd::Library(folders, options));
    }

    pub fn rescan(&self) {
        let _ = self.tx.send(Cmd::Rescan);
    }

    /// Drops the whole index and indexes everything again.
    pub fn rebuild(&self) {
        let _ = self.tx.send(Cmd::Rebuild);
    }

    /// Stops indexing, saving progress, and waits until the index is released.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A file that needs (re)indexing.
struct Candidate {
    path: PathBuf,
    root: usize,
    /// `None` when only the name is indexed.
    kind: Option<Kind>,
    meta: FileMeta,
    /// The file is already in the index and must be replaced.
    replace: bool,
}

struct Built {
    path: String,
    doc: TantivyDocument,
    replace: bool,
}

/// Turns files into documents; shared with the threads of a running sync.
struct Extraction {
    extractor: Extractor,
    pool: rayon::ThreadPool,
}

impl Extraction {
    /// `None` when the file should simply be tried again later.
    fn build(&self, engine: &Engine, roots: &[Folder], c: Candidate) -> Option<Built> {
        let text;
        let content = match c.kind {
            None => Content::NameOnly,
            Some(kind) => match self.extractor.extract(&c.path, kind) {
                Ok(t) => {
                    text = t;
                    Content::Text(&text)
                }
                Err(e) if e.transient => {
                    log::warn!("{}: {e}", c.path.display());
                    return None;
                }
                Err(e) => {
                    log::info!("{}: {e}", c.path.display());
                    Content::Failed(e.failure)
                }
            },
        };
        let root = roots.get(c.root).map(|f| f.path.as_path()).unwrap_or(Path::new("/"));
        Some(Built {
            path: c.path.to_string_lossy().into_owned(),
            doc: engine.make_doc(&c.path, root, c.meta, content),
            replace: c.replace,
        })
    }
}

struct Worker {
    engine: Arc<Engine>,
    extraction: Arc<Extraction>,
    writer: IndexWriter,
    rx: Receiver<Cmd>,
    watcher: Option<RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    watch_full: bool,
    roots: Vec<Folder>,
    options: IndexOptions,
    pending: VecDeque<Cmd>,
    last_commit: Instant,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
}

impl Worker {
    fn run(mut self) {
        loop {
            let cmd = match self.pending.pop_front() {
                Some(cmd) => cmd,
                None => match self.rx.recv_timeout(RESCAN_EVERY) {
                    Ok(cmd) => cmd,
                    Err(RecvTimeoutError::Timeout) => Cmd::Rescan,
                    Err(RecvTimeoutError::Disconnected) => Cmd::Stop,
                },
            };
            match cmd {
                Cmd::Library(roots, options) => {
                    self.roots = roots;
                    self.options = options;
                    self.sync();
                }
                Cmd::Rescan => self.sync(),
                Cmd::Rebuild => {
                    let _ = self.writer.delete_all_documents();
                    self.commit();
                    self.sync();
                }
                Cmd::Changed(paths) => self.changed(paths),
                Cmd::Stop => {
                    self.commit();
                    return;
                }
            }
        }
    }

    fn emit_idle(&self) {
        let engine = &self.engine;
        let status = Status {
            docs: engine.num_docs(),
            failed: engine.failed_count(),
            folders: self.roots.iter().map(|f| engine.count_in(f)).collect(),
        };
        (self.on_event)(Event::Idle(status));
    }

    fn commit(&mut self) {
        self.last_commit = Instant::now();
        if let Err(e) = self.writer.commit() {
            log::error!("commit failed: {e}");
            (self.on_event)(Event::Error(format!("Could not save the index: {e}")));
        }
        self.engine.reload();
    }

    fn add(&mut self, built: Built) {
        if built.replace {
            self.writer.delete_term(self.engine.path_term(&built.path));
        }
        if let Err(e) = self.writer.add_document(built.doc) {
            log::error!("add {}: {e}", built.path);
        }
    }

    /// Queues commands that arrived meanwhile. Returns true if a full sync should stop.
    fn poll_interrupt(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => self.pending.push_back(cmd),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending.push_back(Cmd::Stop);
                    break;
                }
            }
        }
        self.pending.iter().any(Cmd::supersedes_sync)
    }

    fn sync(&mut self) {
        let finished = self.sync_pipeline();
        self.commit();
        self.extraction.extractor.release_idle();
        if finished {
            self.emit_idle();
        }
    }

    /// Returns false if the sync was interrupted by a newer command.
    fn sync_pipeline(&mut self) -> bool {
        let existing = self.engine.existing();
        let cancel = AtomicBool::new(false);
        let scanned = AtomicUsize::new(0);
        let queued = AtomicUsize::new(0);
        let scan_done = AtomicBool::new(false);
        let (cand_tx, cand_rx) = mpsc::sync_channel::<Candidate>(1024);
        // Small: each document may hold megabytes of text.
        let (doc_tx, doc_rx) = mpsc::sync_channel::<Built>(2 * self.extraction.pool.current_num_threads());

        let roots = self.roots.clone();
        let options = self.options.clone();
        let engine = self.engine.clone();
        let extraction = self.extraction.clone();
        let mut done = 0;
        let mut last_commit = Instant::now();
        let mut last_progress = Instant::now();
        let mut interrupted = false;

        let scan = std::thread::scope(|s| {
            let scanner = s.spawn(|| {
                crate::priority::lower_current_thread();
                let out = scan(&roots, &options, &existing, &cand_tx, &cancel, &scanned, &queued);
                drop(cand_tx);
                scan_done.store(true, Ordering::Release);
                out
            });
            s.spawn(|| {
                extraction.pool.install(|| {
                    cand_rx.into_iter().par_bridge().for_each_with(doc_tx, |tx, c| {
                        if !cancel.load(Ordering::Relaxed) {
                            if let Some(built) = extraction.build(&engine, &roots, c) {
                                let _ = tx.send(built);
                            }
                        }
                    })
                })
            });

            loop {
                match doc_rx.recv_timeout(PROGRESS_EVERY) {
                    Ok(built) => {
                        self.add(built);
                        done += 1;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if last_progress.elapsed() >= PROGRESS_EVERY {
                    last_progress = Instant::now();
                    if !interrupted && self.poll_interrupt() {
                        interrupted = true;
                        cancel.store(true, Ordering::Relaxed);
                    }
                    (self.on_event)(Event::Progress(Progress {
                        scanning: !scan_done.load(Ordering::Acquire),
                        scanned: scanned.load(Ordering::Relaxed),
                        total: queued.load(Ordering::Relaxed),
                        done,
                    }));
                }
                if last_commit.elapsed() >= COMMIT_EVERY {
                    self.commit();
                    last_commit = Instant::now();
                }
            }
            scanner.join().expect("scanner thread")
        });
        if interrupted {
            return false;
        }

        // Forget files that disappeared, but keep those in folders that are currently
        // unavailable (an unplugged drive) until they come back.
        let offline: Vec<&Path> = self.roots.iter().map(|f| f.path.as_path()).filter(|p| !p.is_dir()).collect();
        for path in existing.keys() {
            if !scan.seen.contains(path) && !offline.iter().any(|r| Path::new(path).starts_with(r)) {
                self.writer.delete_term(self.engine.path_term(path));
            }
        }
        self.update_watches(scan.dirs);
        true
    }

    fn update_watches(&mut self, dirs: HashSet<PathBuf>) {
        let Some(watcher) = self.watcher.as_mut() else { return };
        for gone in self.watched.difference(&dirs) {
            let _ = watcher.unwatch(gone);
        }
        self.watched.retain(|d| dirs.contains(d));
        for dir in dirs {
            if self.watch_full {
                break;
            }
            if self.watched.contains(&dir) {
                continue;
            }
            match watcher.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(dir);
                }
                Err(e) if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) => {
                    log::warn!("inotify watch limit reached; relying on periodic rescans");
                    self.watch_full = true;
                }
                Err(_) => {}
            }
        }
    }

    /// Applies file system changes, after waiting briefly for related events to settle.
    fn changed(&mut self, first: Vec<PathBuf>) {
        let mut paths: HashSet<PathBuf> = first.into_iter().collect();
        // Changes queued while busy are handled together.
        self.pending.retain(|cmd| match cmd {
            Cmd::Changed(more) => {
                paths.extend(more.iter().cloned());
                false
            }
            _ => true,
        });
        // Wait for related events to settle, and commit at most every MIN_COMMIT_GAP, so a
        // file that is written continuously can't keep the indexer busy.
        let deadline = Instant::now() + DEBOUNCE.max(MIN_COMMIT_GAP.saturating_sub(self.last_commit.elapsed()));
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(left) {
                Ok(Cmd::Changed(more)) => paths.extend(more),
                Ok(other) => self.pending.push_back(other),
                Err(_) => break,
            }
        }

        let engine = self.engine.clone();
        let mut todo = Vec::new();
        let mut new_dirs = HashSet::new();
        for path in paths {
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.is_dir() => {
                    let Some(root) = self.roots.iter().position(|f| f.include_subfolders && path.starts_with(&f.path)) else {
                        continue;
                    };
                    if self.options.skips_below(&self.roots[root].path, &path) {
                        continue;
                    }
                    let folder = Folder { path: path.clone(), include_subfolders: true };
                    walk(&folder, &self.options, &mut new_dirs, &mut |p, kind, meta| {
                        todo.extend(needs_indexing(p, root, kind, meta, engine.indexed_meta(p)));
                        true
                    });
                }
                Ok(m) if m.is_file() => {
                    let Some(parent) = path.parent() else { continue };
                    let Some(root) = self.roots.iter().position(|f| f.covers_dir(parent)) else { continue };
                    if self.options.skips_below(&self.roots[root].path, &path) {
                        continue;
                    }
                    if let Some((kind, meta)) = classify(&path, &m, &self.options) {
                        todo.extend(needs_indexing(&path, root, kind, meta, engine.indexed_meta(&path)));
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    self.writer.delete_term(self.engine.path_term(&path.to_string_lossy()));
                    self.engine.delete_under(&self.writer, &path);
                    if self.watched.remove(&path) {
                        if let Some(w) = self.watcher.as_mut() {
                            let _ = w.unwatch(&path);
                        }
                    }
                }
            }
        }
        if !new_dirs.is_empty() {
            let mut all = self.watched.clone();
            all.extend(new_dirs);
            self.update_watches(all);
        }
        // In chunks, so a large folder moved into the library never holds all its text in memory.
        let total = todo.len();
        let mut done = 0;
        let extraction = self.extraction.clone();
        let roots = self.roots.clone();
        let mut todo = todo.into_iter();
        loop {
            let chunk: Vec<Candidate> = todo.by_ref().take(CHANGE_CHUNK).collect();
            if chunk.is_empty() {
                break;
            }
            done += chunk.len();
            let built: Vec<Built> =
                extraction.pool.install(|| chunk.into_par_iter().filter_map(|c| extraction.build(&engine, &roots, c)).collect());
            for b in built {
                self.add(b);
            }
            if total > CHANGE_CHUNK {
                (self.on_event)(Event::Progress(Progress { scanning: false, scanned: total, total, done }));
            }
        }
        self.commit();
        self.extraction.extractor.release_idle();
        if !self.pending.iter().any(Cmd::supersedes_sync) {
            self.emit_idle();
        }
    }
}

/// A candidate if the file isn't indexed yet or changed since (`known` is what the index has).
fn needs_indexing(path: &Path, root: usize, kind: Option<Kind>, meta: FileMeta, known: Option<FileMeta>) -> Option<Candidate> {
    (known != Some(meta)).then(|| Candidate { path: path.to_owned(), root, kind, meta, replace: known.is_some() })
}

struct ScanResult {
    seen: HashSet<String>,
    dirs: HashSet<PathBuf>,
}

/// Walks every library folder, queueing files whose metadata differs from the index.
fn scan(
    roots: &[Folder],
    options: &IndexOptions,
    existing: &HashMap<String, FileMeta>,
    queue: &SyncSender<Candidate>,
    cancel: &AtomicBool,
    scanned: &AtomicUsize,
    queued: &AtomicUsize,
) -> ScanResult {
    let mut seen = HashSet::new();
    let mut dirs = HashSet::new();
    for (root, folder) in roots.iter().enumerate() {
        walk(folder, options, &mut dirs, &mut |path, kind, meta| {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            scanned.fetch_add(1, Ordering::Relaxed);
            let key = path.to_string_lossy().into_owned();
            if let Some(c) = needs_indexing(path, root, kind, meta, existing.get(&key).copied()) {
                queued.fetch_add(1, Ordering::Relaxed);
                if queue.send(c).is_err() {
                    return false;
                }
            }
            seen.insert(key);
            true
        });
    }
    ScanResult { seen, dirs }
}

/// Every core helps (at idle priority), but with at least ~1 GiB of RAM per thread, since
/// each may be parsing a large document.
fn extraction_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let gib = crate::engine::total_memory().map_or(4, |m| (m >> 30) as usize);
    cores.min(gib).max(1)
}

/// Decides whether and how a file is indexed.
fn classify(path: &Path, m: &std::fs::Metadata, options: &IndexOptions) -> Option<(Option<Kind>, FileMeta)> {
    let mtime = m.modified().ok()?.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let meta = FileMeta { mtime, size: m.len() };
    match extract::kind_for(path) {
        Some(kind) if extract::is_indexable(kind, m.len()) => Some((Some(kind), meta)),
        // Too large to read, or not a document: still findable by name.
        _ if options.file_names => Some((None, meta)),
        _ => None,
    }
}

/// Walks a library folder, skipping hidden entries (unless enabled), dependency folders and
/// cache directories. Records watched directories in `dirs` and reports indexable files to
/// `found`, which returns false to stop the walk.
fn walk(
    folder: &Folder,
    options: &IndexOptions,
    dirs: &mut HashSet<PathBuf>,
    found: &mut dyn FnMut(&Path, Option<Kind>, FileMeta) -> bool,
) {
    let depth = if folder.include_subfolders { usize::MAX } else { 1 };
    let walker = walkdir::WalkDir::new(&folder.path).follow_links(false).max_depth(depth).into_iter().filter_entry(|e| {
        if e.depth() == 0 {
            // Name rules don't apply to the library folder itself, excluded folders do.
            return !options.excluded_folders.iter().any(|f| e.path().starts_with(f));
        }
        if options.skips(e.path()) {
            return false;
        }
        // Directories tagged as caches (CACHEDIR.TAG) hold nothing worth searching.
        !(e.file_type().is_dir() && e.path().join("CACHEDIR.TAG").exists())
    });
    for entry in walker.filter_map(Result::ok) {
        let ft = entry.file_type();
        if ft.is_dir() {
            if folder.include_subfolders || entry.depth() == 0 {
                dirs.insert(entry.into_path());
            }
        } else if ft.is_file() {
            let Ok(m) = entry.metadata() else { continue };
            if let Some((kind, meta)) = classify(entry.path(), &m, options) {
                if !found(entry.path(), kind, meta) {
                    return;
                }
            }
        }
    }
}
