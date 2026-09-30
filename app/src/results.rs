//! Result rows: highlighted name and snippet, a context menu, and drag and drop.

use std::path::Path;

use adw::prelude::*;
use filefind_core::{Hit, Segment};
use gtk::{gdk, gio, glib, pango};

use crate::i18n::tr;
use crate::window::display_path;

pub fn row(hit: &Hit, accent: &str, menu: &gtk::PopoverMenu) -> gtk::ListBoxRow {
    let path = Path::new(&hit.path);
    let icon = gtk::Image::from_gicon(&file_icon(path));
    icon.set_pixel_size(32);
    icon.set_valign(gtk::Align::Start);
    icon.set_margin_top(2);

    let name = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(pango::EllipsizeMode::Middle).build();
    name.set_markup(&markup(&hit.name, &format!("<span foreground=\"{accent}\">"), "</span>", false));
    name.add_css_class("result-title");
    let date = gtk::Label::new(Some(&fmt_date(hit.mtime)));
    date.add_css_class("result-date");
    date.set_tooltip_text(Some(&glib::format_size(hit.size)));
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    top.append(&name);
    if hit.matches > 0 {
        let badge = gtk::Label::builder()
            .label(crate::i18n::fmt_count(hit.matches as u64))
            .tooltip_text(crate::i18n::ntr("{} match", "{} matches", hit.matches as u64).replace("{}", &hit.matches.to_string()))
            .valign(gtk::Align::Center)
            .build();
        badge.add_css_class("match-badge");
        top.append(&badge);
    }
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

    let click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
    let target = hit.path.clone();
    click.connect_pressed(glib::clone!(#[weak] row, #[strong] menu, #[strong] target, move |_, _, x, y| {
        show_menu(&menu, &row, x, y, &target);
    }));
    row.add_controller(click);
    let long_press = gtk::GestureLongPress::new();
    long_press.connect_pressed(glib::clone!(#[weak] row, #[strong] menu, move |_, x, y| {
        show_menu(&menu, &row, x, y, &target);
    }));
    row.add_controller(long_press);

    // Drag the file into other apps (mail, chat, file managers).
    let drag = gtk::DragSource::new();
    drag.set_actions(gdk::DragAction::COPY);
    let file = gio::File::for_path(path);
    drag.set_content(Some(&gdk::ContentProvider::for_value(&gdk::FileList::from_array(&[file]).to_value())));
    drag.connect_drag_begin(glib::clone!(#[weak] icon, move |source, _| {
        source.set_icon(Some(&gtk::WidgetPaintable::new(Some(&icon))), 16, 16);
    }));
    row.add_controller(drag);
    row
}

pub fn file_icon(path: &Path) -> gio::Icon {
    let (content_type, _) = gio::content_type_guess(Some(path), None::<&[u8]>);
    gio::content_type_get_icon(&content_type)
}

/// A single context menu shared by all rows; each row fills it in when opened.
pub fn context_menu(anchor: &impl IsA<gtk::Widget>) -> gtk::PopoverMenu {
    let menu = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
    menu.set_has_arrow(false);
    menu.set_halign(gtk::Align::Start);
    menu.set_parent(anchor);
    menu
}

fn show_menu(popover: &gtk::PopoverMenu, row: &gtk::ListBoxRow, x: f64, y: f64, path: &str) {
    let Some(anchor) = popover.parent() else { return };
    if let Some(list) = row.parent().and_downcast::<gtk::ListBox>() {
        list.select_row(Some(row));
    }
    let menu = gio::Menu::new();
    let actions = [
        (tr("Open"), "win.open"),
        (tr("Preview"), "win.preview"),
        (tr("Show in Folder"), "win.show-in-folder"),
        (tr("Copy Path"), "win.copy-path"),
    ];
    for (label, action) in actions {
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

pub fn fmt_date(mtime: u64) -> String {
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
