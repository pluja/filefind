# Filefind

Find any file by what's inside it. Add folders to your library, then type:
results appear as you type, with typo-tolerant full-text search.

- Searches inside PDF, DOC/DOCX, ODT/ODS/ODP, XLSX, PPTX, RTF, EPUB, HTML, Markdown, text and code
- Other files (photos, music, archives…) are found by name
- Fuzzy and prefix matching, other word forms (English and Spanish), accent-insensitive
- Filters by type and date, sorting, and a search syntax: `"phrase"`, `-word`, `type:pdf`, `in:folder`, `name:word`
- Quick preview (Space) with every match highlighted; CSV as a table, Markdown formatted
- Results in GNOME Shell's search and KDE's KRunner
- Keeps the index up to date as files change; optional background indexing
- Exclude folders (like `~/Downloads/private`) or names and patterns (like `*.log`) in Settings → Library
- Indexing runs at idle CPU and disk priority; risky parsers (PDF, legacy Word) run in
  isolated helper processes with a time limit and memory cap
- GTK4 + libadwaita, English and Spanish; works on GNOME and KDE Plasma; read-only file access

## Development

Everything builds inside the GNOME Flatpak SDK, so the host only needs `flatpak`
(and `cargo` for the core tests).

```sh
make setup      # once: install the GNOME SDK, Rust extension, flatpak-builder
make dev        # build & run a debug version (data kept in .dev/)
make dev-es     # same, in Spanish
make demo       # run against a small sample library
make test       # all tests
make flatpak    # build and install the Flatpak for your user
make bundle     # produce filefind.flatpak to share
make            # list all commands
```

`cargo run --release -p filefind-core --example bench -- <folder>` measures indexing speed.

Layout: `core/` is the search engine (Tantivy index, text extraction, background
indexer, query syntax); `app/` is the GTK app; `po/` holds translations; `data/` the
desktop integration files.

To add a language: add its code to `po/LINGUAS` and `app/src/i18n.rs`, run `make po`,
then translate `po/<code>.po`.
