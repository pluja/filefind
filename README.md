# Filefind

Find any file by what's inside it. Add folders to your library, then type:
results appear as you type, with typo-tolerant (fuzzy) full-text search.

- Searches inside PDF, DOC/DOCX, ODT/ODS/ODP, XLSX, PPTX, RTF, EPUB, HTML, Markdown, text and source code
- Fuzzy matching, prefix matching while typing, `"exact phrases"`, accent-insensitive
- Keeps the index up to date as files change (inotify + periodic rescan)
- Risky parsers (PDF, legacy Word) run in an isolated helper process with a timeout and memory limit
- Native GTK4 + libadwaita UI, English and Spanish (follows the system language)
- Works on GNOME and KDE Plasma; read-only access to your files

## Development

Everything builds inside the GNOME Flatpak SDK, so the host only needs `flatpak`
(and `cargo` for the core tests).

```sh
make setup      # once: install the GNOME SDK, Rust extension, flatpak-builder
make dev        # build & run a debug version (data kept in .dev/)
make dev-es     # same, in Spanish
make demo       # run against a small sample library
make test       # core tests: extraction, indexing, fuzzy search, live updates
make flatpak    # build and install the Flatpak for your user
make bundle     # produce filefind.flatpak to share
make            # list all commands
```

Layout: `core/` is the search engine library (Tantivy index, text extraction,
background indexer); `app/` is the GTK app; `po/` holds translations.

To add a language: add its code to `po/LINGUAS`, run `make po`, then translate `po/<code>.po`.
