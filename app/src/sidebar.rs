//! The library sidebar: the folders Filefind searches, and the index status.

use std::rc::Rc;

use adw::prelude::*;
use gtk::pango;

use crate::i18n::{fmt_count, ntr, tr};
use crate::window::{display_name, display_path, State};

pub struct Sidebar {
    pub widget: adw::ToolbarView,
    folders: gtk::ListBox,
    status: gtk::Label,
}

impl Sidebar {
    pub fn new() -> Sidebar {
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new(&tr("Library"), "")));

        let folders = gtk::ListBox::new();
        folders.add_css_class("navigation-sidebar");
        folders.set_selection_mode(gtk::SelectionMode::None);
        let placeholder = gtk::Label::builder()
            .label(tr("Add the folders you want to search"))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .margin_top(24)
            .margin_start(18)
            .margin_end(18)
            .build();
        placeholder.add_css_class("dim-label");
        folders.set_placeholder(Some(&placeholder));
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&folders)
            .vexpand(true)
            .build();

        let add = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label(tr("Add Folder…")).use_underline(true).build())
            .action_name("win.add-folder")
            .build();
        add.add_css_class("sidebar-add");
        let status = gtk::Label::builder().xalign(0.5).ellipsize(pango::EllipsizeMode::End).build();
        status.add_css_class("sidebar-status");
        let footer = gtk::Box::new(gtk::Orientation::Vertical, 6);
        footer.add_css_class("sidebar-footer");
        footer.append(&add);
        footer.append(&status);

        let widget = adw::ToolbarView::new();
        widget.add_top_bar(&header);
        widget.set_content(Some(&scroller));
        widget.add_bottom_bar(&footer);
        Sidebar { widget, folders, status }
    }

    pub fn refresh(&self, state: &Rc<State>) {
        self.folders.remove_all();
        let library = state.library.borrow().clone();
        for folder in library.folders {
            let available = folder.is_dir();
            let icon = gtk::Image::from_icon_name(if available { "folder-symbolic" } else { "folder-remote-symbolic" });
            let name = gtk::Label::builder().label(display_name(&folder)).xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
            let location = if available { display_path(&folder) } else { tr("Not available") };
            let path = gtk::Label::builder().label(&location).xalign(0.0).ellipsize(pango::EllipsizeMode::Middle).build();
            path.add_css_class("sidebar-path");
            let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
            text.set_hexpand(true);
            text.append(&name);
            text.append(&path);

            let remove = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text(tr("Remove from Library"))
                .valign(gtk::Align::Center)
                .build();
            remove.add_css_class("flat");
            remove.add_css_class("circular");
            remove.add_css_class("sidebar-remove");
            let weak = Rc::downgrade(state);
            let target = folder.clone();
            remove.connect_clicked(move |_| {
                if let Some(state) = weak.upgrade() {
                    state.remove_folder(&target);
                }
            });

            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row_box.append(&icon);
            row_box.append(&text);
            row_box.append(&remove);
            let row = gtk::ListBoxRow::builder().child(&row_box).activatable(false).build();
            row.set_tooltip_text(Some(&folder.display().to_string()));
            self.folders.append(&row);
        }

        let docs = state.doc_count.get();
        let status = if state.is_indexing() {
            tr("Indexing…")
        } else {
            ntr("{} file indexed", "{} files indexed", docs).replace("{}", &fmt_count(docs))
        };
        self.status.set_label(&status);
    }
}
