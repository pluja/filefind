//! Quick preview: the selected file's text with every match highlighted, or the image itself.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use sourceview5::prelude::{BufferExt as _, ViewExt as _};
use filefind_core::{Category, Hit};
use gtk::{gdk, gio, glib, pango};

use crate::backend::Backend;
use crate::i18n::tr;
use crate::results::{file_icon, fmt_date};
use crate::window::display_path;

/// Very long documents are shown up to this size; the text view stays responsive.
const MAX_PREVIEW_BYTES: usize = 2 * 1024 * 1024;

pub struct Preview {
    pub widget: adw::ToolbarView,
    backend: Rc<Backend>,
    outer: gtk::Stack,
    icon: gtk::Image,
    name: gtk::Label,
    details: gtk::Label,
    stack: gtk::Stack,
    text: sourceview5::View,
    buffer: sourceview5::Buffer,
    table: gtk::ScrolledWindow,
    picture: gtk::Picture,
    matches_bar: gtk::Box,
    matches_label: gtk::Label,
    /// Previous/next buttons; tables only report how many cells match.
    match_nav: gtk::Box,
    matches: RefCell<Vec<(gtk::TextMark, gtk::TextMark)>>,
    current: Cell<usize>,
    path: RefCell<Option<String>>,
    /// Ignores loads that finish after another file was selected.
    generation: Cell<u64>,
}

impl Preview {
    pub fn new(backend: &Rc<Backend>) -> Rc<Preview> {
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new(&tr("Preview"), "")));

        let icon = gtk::Image::builder().pixel_size(48).build();
        let name = gtk::Label::builder().wrap(true).wrap_mode(pango::WrapMode::WordChar).xalign(0.0).build();
        name.add_css_class("title-4");
        let details = gtk::Label::builder().wrap(true).xalign(0.0).build();
        details.add_css_class("dim-label");
        details.add_css_class("caption");
        let titles = gtk::Box::new(gtk::Orientation::Vertical, 2);
        titles.set_valign(gtk::Align::Center);
        titles.append(&name);
        titles.append(&details);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        top.append(&icon);
        top.append(&titles);

        let open = gtk::Button::builder().label(tr("Open")).action_name("win.open-previewed").build();
        open.add_css_class("pill");
        open.add_css_class("suggested-action");
        let reveal = gtk::Button::builder().label(tr("Show in Folder")).action_name("win.reveal-previewed").build();
        reveal.add_css_class("pill");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.append(&open);
        buttons.append(&reveal);

        let matches_label = gtk::Label::new(None);
        matches_label.set_hexpand(true);
        matches_label.set_xalign(0.0);
        matches_label.add_css_class("caption-heading");
        let previous = gtk::Button::builder().icon_name("go-up-symbolic").tooltip_text(tr("Previous Match")).build();
        let next = gtk::Button::builder().icon_name("go-down-symbolic").tooltip_text(tr("Next Match")).build();
        previous.add_css_class("flat");
        next.add_css_class("flat");
        let matches_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        matches_bar.add_css_class("preview-matches");
        let match_nav = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        match_nav.append(&previous);
        match_nav.append(&next);
        matches_bar.append(&matches_label);
        matches_bar.append(&match_nav);

        let buffer = sourceview5::Buffer::new(None);
        follow_dark_mode(&buffer);
        let text = sourceview5::View::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(16)
            .right_margin(16)
            .top_margin(12)
            .bottom_margin(24)
            .build();
        text.add_css_class("preview-text");
        buffer.create_tag(Some("match"), &[("background-rgba", &gdk::RGBA::new(1.0, 0.85, 0.2, 0.45))]);
        buffer.create_tag(Some("current"), &[("background-rgba", &gdk::RGBA::new(1.0, 0.6, 0.0, 0.8))]);
        crate::render::create_tags(buffer.upcast_ref());
        let text_scroller = gtk::ScrolledWindow::builder().child(&text).vexpand(true).build();

        let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Contain).can_shrink(true).vexpand(true).build();
        picture.add_css_class("preview-picture");

        let loading = adw::Spinner::builder().width_request(32).height_request(32).halign(gtk::Align::Center).valign(gtk::Align::Center).build();
        let nothing = adw::StatusPage::builder()
            .icon_name("view-reveal-symbolic")
            .title(tr("No Preview"))
            .description(tr("This file has no text to show."))
            .build();
        nothing.add_css_class("compact");
        let empty = adw::StatusPage::builder()
            .icon_name("view-reveal-symbolic")
            .title(tr("Quick Preview"))
            .description(tr("Select a result to see what's inside."))
            .build();
        empty.add_css_class("compact");

        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
        stack.add_named(&text_scroller, Some("text"));
        stack.add_named(&picture, Some("picture"));
        let table = gtk::ScrolledWindow::builder().vexpand(true).build();
        stack.add_named(&table, Some("table"));
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&nothing, Some("nothing"));

        let file_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        file_box.add_css_class("preview-header");
        file_box.append(&top);
        file_box.append(&buttons);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&file_box);
        body.append(&matches_bar);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&stack);

        let outer = gtk::Stack::new();
        outer.add_named(&empty, Some("empty"));
        outer.add_named(&body, Some("file"));
        outer.set_visible_child_name("empty");

        let widget = adw::ToolbarView::new();
        widget.add_top_bar(&header);
        widget.set_content(Some(&outer));

        let preview = Rc::new(Preview {
            widget,
            backend: backend.clone(),
            outer,
            icon,
            name,
            details,
            stack,
            text,
            buffer,
            table,
            picture,
            matches_bar,
            matches_label,
            match_nav,
            matches: RefCell::new(Vec::new()),
            current: Cell::new(0),
            path: RefCell::new(None),
            generation: Cell::new(0),
        });
        previous.connect_clicked(glib::clone!(#[weak] preview, move |_| preview.step(-1)));
        next.connect_clicked(glib::clone!(#[weak] preview, move |_| preview.step(1)));
        preview
    }

    pub fn path(&self) -> Option<String> {
        self.path.borrow().clone()
    }

    pub fn clear(&self) {
        self.generation.set(self.generation.get() + 1);
        self.path.replace(None);
        self.outer.set_visible_child_name("empty");
    }

    /// Shows `hit`, highlighting the normalized search `terms`.
    pub fn show(self: &Rc<Self>, hit: &Hit, terms: Vec<String>) {
        if self.path.borrow().as_deref() == Some(hit.path.as_str()) {
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.path.replace(Some(hit.path.clone()));
        self.outer.set_visible_child_name("file");

        let path = PathBuf::from(&hit.path);
        self.icon.set_from_gicon(&file_icon(&path));
        self.name.set_label(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let folder = path.parent().map(display_path).unwrap_or_default();
        self.details.set_label(&format!("{folder}\n{} · {}", glib::format_size(hit.size), fmt_date(hit.mtime)));
        self.matches_bar.set_visible(false);
        self.stack.set_visible_child_name("loading");

        let this = self.clone();
        let category = hit.category;
        let extension = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        glib::spawn_future_local(async move {
            if category == Category::Image {
                this.show_image(path, generation).await;
            } else {
                let path_for_language = path.clone();
                let text = this.backend.read_text(path).await;
                if this.generation.get() == generation {
                    this.show_text(text, &terms, &path_for_language, &extension);
                }
            }
        });
    }

    async fn show_image(&self, path: PathBuf, generation: u64) {
        let texture = gio::spawn_blocking(move || gdk::Texture::from_file(&gio::File::for_path(&path)).ok())
            .await
            .ok()
            .flatten();
        if self.generation.get() != generation {
            return;
        }
        match texture {
            Some(texture) => {
                self.picture.set_paintable(Some(&texture));
                self.stack.set_visible_child_name("picture");
            }
            None => self.stack.set_visible_child_name("nothing"),
        }
    }

    fn show_text(&self, text: Option<String>, terms: &[String], path: &std::path::Path, extension: &str) {
        let Some(mut text) = text.filter(|t| !t.trim().is_empty()) else {
            self.stack.set_visible_child_name("nothing");
            return;
        };
        filefind_core::extract::truncate_at_char_boundary(&mut text, MAX_PREVIEW_BYTES);
        let buffer = self.buffer.clone();
        buffer.set_language(None);
        self.text.set_monospace(false);
        self.text.set_show_line_numbers(false);
        self.text.set_wrap_mode(gtk::WrapMode::WordChar);
        match extension {
            "csv" => return self.show_table(&text, ',', terms),
            // Spreadsheets are extracted as tab-separated rows.
            "tsv" | "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" | "ots" => return self.show_table(&text, '\t', terms),
            // HTML and EPUB are extracted as Markdown-like text.
            "md" | "markdown" | "html" | "htm" | "xhtml" | "epub" => crate::render::markdown(buffer.upcast_ref(), &text),
            _ => {
                buffer.set_text(&text);
                let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                let language = sourceview5::LanguageManager::default().guess_language(file_name.as_deref(), None::<&str>);
                if let Some(language) = language {
                    buffer.set_language(Some(&language));
                    // Code keeps its lines; the view scrolls sideways instead.
                    self.text.set_monospace(true);
                    self.text.set_show_line_numbers(true);
                    self.text.set_wrap_mode(gtk::WrapMode::None);
                }
            }
        }
        // Highlight what is shown, which differs from the source for rendered Markdown.
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();

        let ranges = filefind_core::engine::highlight_ranges(&text, terms);
        let mut marks = Vec::with_capacity(ranges.len());
        // Byte offsets to character offsets, walking the text once.
        let mut chars = 0i32;
        let mut byte = 0usize;
        for (from, to) in ranges.into_iter().take(5000) {
            chars += text[byte..from].chars().count() as i32;
            let start = chars;
            chars += text[from..to].chars().count() as i32;
            byte = to;
            let (a, b) = (buffer.iter_at_offset(start), buffer.iter_at_offset(chars));
            buffer.apply_tag_by_name("match", &a, &b);
            marks.push((buffer.create_mark(None, &a, true), buffer.create_mark(None, &b, false)));
        }
        self.matches_bar.set_visible(!marks.is_empty());
        self.match_nav.set_visible(true);
        self.matches.replace(marks);
        self.current.set(0);
        self.stack.set_visible_child_name("text");
        self.focus_match();
    }

    fn show_table(&self, text: &str, separator: char, terms: &[String]) {
        const MAX_ROWS: usize = 500;
        const MAX_COLUMNS: usize = 40;
        let grid = gtk::Grid::builder().halign(gtk::Align::Start).valign(gtk::Align::Start).build();
        grid.add_css_class("preview-table");
        let rows = crate::render::parse_delimited(text, separator, MAX_ROWS);
        // Each column is as wide as its content needs, within limits; wider tables scroll.
        let mut widths = vec![0usize; rows.iter().map(Vec::len).max().unwrap_or(0).min(MAX_COLUMNS)];
        for row in &rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        let mut found = 0;
        let rows: Vec<&Vec<String>> = rows.iter().filter(|row| row.iter().any(|c| !c.trim().is_empty())).collect();
        for (r, row) in rows.iter().enumerate() {
            for (c, cell) in row.iter().take(MAX_COLUMNS).enumerate() {
                let label = gtk::Label::builder()
                    .label(cell.as_str())
                    .xalign(0.0)
                    .ellipsize(pango::EllipsizeMode::End)
                    .width_chars(widths[c].clamp(3, 24) as i32)
                    .max_width_chars(40)
                    .tooltip_text(cell.as_str())
                    .build();
                if r == 0 {
                    label.add_css_class("header");
                }
                if !filefind_core::engine::highlight_ranges(cell, terms).is_empty() {
                    label.add_css_class("match");
                    found += 1;
                }
                grid.attach(&label, c as i32, r as i32, 1, 1);
            }
        }
        self.table.set_child(Some(&grid));
        self.matches.replace(Vec::new());
        self.matches_bar.set_visible(found > 0);
        self.match_nav.set_visible(false);
        self.matches_label.set_label(&crate::i18n::ntr("{} matching cell", "{} matching cells", found).replace("{}", &found.to_string()));
        self.stack.set_visible_child_name("table");
    }

    fn step(&self, delta: i64) {
        let count = self.matches.borrow().len() as i64;
        if count == 0 {
            return;
        }
        self.current.set((self.current.get() as i64 + delta).rem_euclid(count) as usize);
        self.focus_match();
    }

    fn focus_match(&self) {
        let matches = self.matches.borrow();
        let buffer = &self.buffer;
        buffer.remove_tag_by_name("current", &buffer.start_iter(), &buffer.end_iter());
        let Some((start, end)) = matches.get(self.current.get()) else {
            buffer.place_cursor(&buffer.start_iter());
            return;
        };
        let (a, b) = (buffer.iter_at_mark(start), buffer.iter_at_mark(end));
        buffer.apply_tag_by_name("current", &a, &b);
        self.matches_label.set_label(
            &tr("Match {current} of {total}")
                .replace("{current}", &(self.current.get() + 1).to_string())
                .replace("{total}", &matches.len().to_string()),
        );
        // Scrolling needs line heights, which are only known after layout.
        let text = self.text.clone();
        let mark = start.clone();
        glib::idle_add_local_once(move || text.scroll_to_mark(&mark, 0.1, true, 0.0, 0.3));
    }
}

/// Uses the Adwaita syntax colors matching the current light or dark style.
fn follow_dark_mode(buffer: &sourceview5::Buffer) {
    let apply = |buffer: &sourceview5::Buffer, dark: bool| {
        let scheme = sourceview5::StyleSchemeManager::default().scheme(if dark { "Adwaita-dark" } else { "Adwaita" });
        buffer.set_style_scheme(scheme.as_ref());
    };
    let style = adw::StyleManager::default();
    apply(buffer, style.is_dark());
    style.connect_dark_notify(glib::clone!(#[weak] buffer, move |style| apply(&buffer, style.is_dark())));
}
