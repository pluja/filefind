//! Indexing benchmark: `cargo run --release -p filefind-core --example bench -- <dir>`
//!
//! Reports extraction cost per format (single thread) and end-to-end indexing throughput.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use filefind_core::extract::{self, Extractor};
use filefind_core::{Engine, Event, Folder, IndexOptions, SearchRequest, Service};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(filefind_core::helper::SERVER_ARG) {
        std::process::exit(filefind_core::helper::serve());
    }
    let dir = PathBuf::from(args.get(1).expect("usage: bench <dir>")).canonicalize().unwrap();
    let extractor = || match std::env::var_os("BENCH_IN_PROCESS") {
        Some(_) => Extractor::in_process(),
        None => Extractor::with_helper(std::env::current_exe().unwrap()),
    };

    if std::env::var_os("BENCH_SKIP_EXTRACT").is_none() {
        per_format(&dir, &extractor());
    }

    let index_dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(index_dir.path()).unwrap());
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    let service = Service::start(engine.clone(), extractor(), move |e| {
        let _ = tx.send(e);
    });
    service.set_library(vec![Folder::new(dir.clone())], IndexOptions::default());
    let docs = loop {
        match rx.recv_timeout(Duration::from_secs(600)).expect("indexer stalled") {
            Event::Idle(status) => break status.docs,
            Event::Error(e) => panic!("{e}"),
            _ => {}
        }
    };
    let full = started.elapsed();
    println!("\nfull index: {docs} files in {full:.2?} ({:.0} files/s)", docs as f64 / full.as_secs_f64());

    let started = Instant::now();
    service.rescan();
    loop {
        if let Event::Idle(_) = rx.recv().unwrap() {
            break;
        }
    }
    println!("rescan (nothing changed): {:.2?}", started.elapsed());

    let started = Instant::now();
    let queries = ["documentation", "licence", "configuration file", "\"free software\"", "instal", "copyrigth"];
    for q in queries {
        engine.search(&SearchRequest::text(q));
    }
    println!("search: {:.2?} per query", started.elapsed() / queries.len() as u32);
}

/// Extracts every file once on this thread and reports time per format.
fn per_format(dir: &Path, extractor: &Extractor) {
    let mut stats: BTreeMap<String, (usize, Duration, usize)> = BTreeMap::new();
    for entry in walkdir::WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        let Some(kind) = extract::kind_for(path) else { continue };
        let started = Instant::now();
        let bytes = extractor.extract(path, kind).map(|t| t.len()).unwrap_or(0);
        let s = stats.entry(format!("{kind:?}")).or_default();
        s.0 += 1;
        s.1 += started.elapsed();
        s.2 += bytes;
    }
    println!("{:<8} {:>7} {:>10} {:>12} {:>10}", "format", "files", "total", "per file", "text MB");
    for (kind, (n, time, bytes)) in stats {
        println!(
            "{kind:<8} {n:>7} {:>10.2?} {:>12.2?} {:>10.1}",
            time,
            time / n.max(1) as u32,
            bytes as f64 / 1e6
        );
    }
}
