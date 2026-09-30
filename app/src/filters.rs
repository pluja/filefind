//! The filter bar: colored file-type chips, a date range and the sort order.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use filefind_core::Category;
use gtk::{gio, glib};

use crate::backend::Backend;
use crate::i18n::tr;
use crate::settings::SortOrder;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Chip {
    Documents,
    Pdf,
    Spreadsheets,
    Presentations,
    Text,
    Images,
    Media,
    Other,
}

impl Chip {
    const ALL: [Chip; 8] =
        [Chip::Documents, Chip::Pdf, Chip::Spreadsheets, Chip::Presentations, Chip::Text, Chip::Images, Chip::Media, Chip::Other];

    fn categories(self) -> &'static [Category] {
        match self {
            Chip::Documents => &[Category::Document],
            Chip::Pdf => &[Category::Pdf],
            Chip::Spreadsheets => &[Category::Spreadsheet],
            Chip::Presentations => &[Category::Presentation],
            Chip::Text => &[Category::Text],
            Chip::Images => &[Category::Image],
            Chip::Media => &[Category::Audio, Category::Video],
            Chip::Other => &[Category::Archive, Category::Other],
        }
    }

    fn label(self) -> String {
        match self {
            Chip::Documents => tr("Documents"),
            Chip::Pdf => tr("PDFs"),
            Chip::Spreadsheets => tr("Sheets"),
            Chip::Presentations => tr("Slides"),
            Chip::Text => tr("Text"),
            Chip::Images => tr("Images"),
            Chip::Media => tr("Media"),
            Chip::Other => tr("Other"),
        }
    }

    fn description(self) -> String {
        match self {
            Chip::Documents => tr("Word, LibreOffice, RTF and EPUB documents"),
            Chip::Pdf => tr("PDF documents"),
            Chip::Spreadsheets => tr("Spreadsheets and CSV files"),
            Chip::Presentations => tr("Presentations"),
            Chip::Text => tr("Text, Markdown, web pages and code"),
            Chip::Images => tr("Photos and images"),
            Chip::Media => tr("Music and videos"),
            Chip::Other => tr("Archives and other files"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Chip::Documents => "x-office-document-symbolic",
            Chip::Pdf => "filefind-pdf-symbolic",
            Chip::Spreadsheets => "x-office-spreadsheet-symbolic",
            Chip::Presentations => "x-office-presentation-symbolic",
            Chip::Text => "text-x-generic-symbolic",
            Chip::Images => "image-x-generic-symbolic",
            Chip::Media => "applications-multimedia-symbolic",
            Chip::Other => "package-x-generic-symbolic",
        }
    }

    fn css(self) -> &'static str {
        match self {
            Chip::Documents => "chip-blue",
            Chip::Pdf => "chip-red",
            Chip::Spreadsheets => "chip-green",
            Chip::Presentations => "chip-orange",
            Chip::Text => "chip-purple",
            Chip::Images => "chip-pink",
            Chip::Media => "chip-teal",
            Chip::Other => "chip-slate",
        }
    }

    /// Only shown when files are also indexed by name.
    fn needs_file_names(self) -> bool {
        matches!(self, Chip::Images | Chip::Media | Chip::Other)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DateRange {
    Any,
    Today,
    Week,
    Month,
    Year,
}

impl DateRange {
    const ALL: [DateRange; 5] = [DateRange::Any, DateRange::Today, DateRange::Week, DateRange::Month, DateRange::Year];

    fn id(self) -> &'static str {
        match self {
            DateRange::Any => "any",
            DateRange::Today => "today",
            DateRange::Week => "week",
            DateRange::Month => "month",
            DateRange::Year => "year",
        }
    }

    fn label(self) -> String {
        match self {
            DateRange::Any => tr("Any Time"),
            DateRange::Today => tr("Today"),
            DateRange::Week => tr("Past Week"),
            DateRange::Month => tr("Past Month"),
            DateRange::Year => tr("Past Year"),
        }
    }

    fn since(self) -> Option<u64> {
        const DAY: u64 = 24 * 3600;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        match self {
            DateRange::Any => None,
            DateRange::Today => glib::DateTime::now_local()
                .and_then(|now| glib::DateTime::from_local(now.year(), now.month(), now.day_of_month(), 0, 0, 0.0))
                .map(|midnight| midnight.to_unix() as u64)
                .ok(),
            DateRange::Week => Some(now.saturating_sub(7 * DAY)),
            DateRange::Month => Some(now.saturating_sub(30 * DAY)),
            DateRange::Year => Some(now.saturating_sub(365 * DAY)),
        }
    }
}

fn sort_label(sort: SortOrder) -> String {
    match sort {
        SortOrder::Relevance => tr("Best Match"),
        SortOrder::Newest => tr("Newest"),
        SortOrder::Oldest => tr("Oldest"),
        SortOrder::Name => tr("Name"),
        SortOrder::Largest => tr("Largest"),
    }
}

pub struct FilterBar {
    pub widget: gtk::Box,
    all: gtk::ToggleButton,
    chips: Vec<(Chip, gtk::ToggleButton)>,
    date: Cell<DateRange>,
    date_label: gtk::Label,
    sort_label: gtk::Label,
    updating: Cell<bool>,
    on_change: RefCell<Option<Rc<dyn Fn()>>>,
}

fn chip_button(label: &str, tooltip: &str, icon: Option<&str>) -> gtk::ToggleButton {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    if let Some(icon) = icon {
        content.append(&gtk::Image::from_icon_name(icon));
    }
    content.append(&gtk::Label::new(Some(label)));
    let button = gtk::ToggleButton::builder().child(&content).tooltip_text(tooltip).build();
    button.add_css_class("chip");
    button
}

fn menu_button(icon: &str, label: &gtk::Label, menu: &gio::Menu, tooltip: &str) -> gtk::MenuButton {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(label);
    content.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
    let button = gtk::MenuButton::builder().child(&content).menu_model(menu).tooltip_text(tooltip).valign(gtk::Align::Start).build();
    button.add_css_class("flat");
    button.add_css_class("filter-menu");
    button
}

impl FilterBar {
    pub fn new(backend: &Rc<Backend>) -> Rc<FilterBar> {
        // Chips wrap onto another line rather than being cut off in narrow windows.
        // Chips wrap onto a second line only when the bar is too narrow for one.
        let chips_box = adw::WrapBox::builder().child_spacing(5).line_spacing(5).hexpand(true).build();
        let all = chip_button(&tr("All"), &tr("All kinds of files"), None);
        all.set_active(true);
        chips_box.append(&all);
        let chips: Vec<(Chip, gtk::ToggleButton)> = Chip::ALL
            .into_iter()
            .map(|chip| {
                let button = chip_button(&chip.label(), &chip.description(), Some(chip.icon()));
                button.add_css_class(chip.css());
                chips_box.append(&button);
                (chip, button)
            })
            .collect();

        let date_menu = gio::Menu::new();
        for range in DateRange::ALL {
            date_menu.append(Some(&range.label()), Some(&format!("filter.date::{}", range.id())));
        }
        let sort_menu = gio::Menu::new();
        for sort in SortOrder::ALL {
            sort_menu.append(Some(&sort_label(sort)), Some(&format!("filter.sort::{}", sort.id())));
        }
        // The menus only show a label once they differ from the default, to leave room for chips.
        let date_label = gtk::Label::builder().label(DateRange::Any.label()).visible(false).build();
        let sort = backend.settings().sort;
        let sort_label_widget = gtk::Label::builder().label(sort_label(sort)).visible(sort != SortOrder::Relevance).build();

        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        widget.add_css_class("filter-bar");
        widget.append(&chips_box);
        widget.append(&menu_button("document-open-recent-symbolic", &date_label, &date_menu, &tr("Modified")));
        widget.append(&menu_button("view-sort-descending-symbolic", &sort_label_widget, &sort_menu, &tr("Sort By")));

        let bar = Rc::new(FilterBar {
            widget,
            all: all.clone(),
            chips,
            date: Cell::new(DateRange::Any),
            date_label,
            sort_label: sort_label_widget,
            updating: Cell::new(false),
            on_change: RefCell::new(None),
        });

        all.connect_toggled(glib::clone!(#[weak] bar, move |all| {
            if bar.updating.get() {
                return;
            }
            bar.updating.set(true);
            // "All" can't be turned off directly; it turns the other chips off.
            all.set_active(true);
            for (_, chip) in &bar.chips {
                chip.set_active(false);
            }
            bar.updating.set(false);
            bar.changed();
        }));
        for (_, button) in &bar.chips {
            button.connect_toggled(glib::clone!(#[weak] bar, move |_| {
                if bar.updating.get() {
                    return;
                }
                bar.updating.set(true);
                let any = bar.chips.iter().any(|(_, b)| b.is_active());
                bar.all.set_active(!any);
                bar.updating.set(false);
                bar.changed();
            }));
        }

        let actions = gio::SimpleActionGroup::new();
        let date = gio::SimpleAction::new_stateful("date", Some(glib::VariantTy::STRING), &DateRange::Any.id().to_variant());
        date.connect_activate(glib::clone!(#[weak] bar, move |action, param| {
            let Some(id) = param.and_then(|p| p.get::<String>()) else { return };
            let Some(range) = DateRange::ALL.into_iter().find(|r| r.id() == id) else { return };
            action.set_state(&id.to_variant());
            bar.date.set(range);
            bar.date_label.set_label(&range.label());
            bar.date_label.set_visible(range != DateRange::Any);
            bar.changed();
        }));
        let sort = gio::SimpleAction::new_stateful("sort", Some(glib::VariantTy::STRING), &backend.settings().sort.id().to_variant());
        sort.connect_activate(glib::clone!(#[weak] bar, #[weak] backend, move |action, param| {
            let Some(order) = param.and_then(|p| p.get::<String>()).and_then(|id| SortOrder::from_id(&id)) else { return };
            action.set_state(&order.id().to_variant());
            bar.sort_label.set_label(&sort_label(order));
            bar.sort_label.set_visible(order != SortOrder::Relevance);
            backend.update_settings(|s| s.sort = order);
            bar.changed();
        }));
        actions.add_action(&date);
        actions.add_action(&sort);
        bar.widget.insert_action_group("filter", Some(&actions));
        bar.sync_with_settings(&backend.settings());
        bar
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.on_change.replace(Some(Rc::new(f)));
    }

    fn changed(&self) {
        let callback = self.on_change.borrow().clone();
        if let Some(f) = callback {
            f();
        }
    }

    pub fn categories(&self) -> Vec<Category> {
        self.chips
            .iter()
            .filter(|(_, b)| b.is_active() && b.is_visible())
            .flat_map(|(chip, _)| chip.categories().iter().copied())
            .collect()
    }

    pub fn modified_since(&self) -> Option<u64> {
        self.date.get().since()
    }

    pub fn sync_with_settings(&self, settings: &crate::settings::Settings) {
        for (chip, button) in &self.chips {
            let visible = settings.file_names || !chip.needs_file_names();
            if button.is_visible() != visible {
                button.set_visible(visible);
                if !visible && button.is_active() {
                    button.set_active(false);
                }
            }
        }
    }

}
