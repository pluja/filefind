//! Search tips: the query syntax, with examples the user can try.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::i18n::tr;

/// Shows the tips; picking an example calls `try_example` with its text.
pub fn present(parent: &impl IsA<gtk::Widget>, try_example: impl Fn(&str) + 'static) {
    let tips = [
        ("budget report", tr("Files with all of these words, anywhere. Small typos are fine.")),
        ("\"net total\"", tr("This exact phrase.")),
        ("invoice -draft", tr("Leave out files that contain a word.")),
        ("type:pdf", tr("Only one kind of file: pdf, doc, sheet, slides, text, image, audio, video, or an extension like docx.")),
        ("in:taxes", tr("Only files in folders with this name.")),
        ("name:invoice", tr("Only look at file names.")),
        ("invoice type:pdf in:2024 -draft", tr("Combine them.")),
    ];
    let dialog = adw::Dialog::builder().title(tr("Search Tips")).content_width(480).content_height(560).build();
    let try_example = Rc::new(try_example);

    let group = adw::PreferencesGroup::builder()
        .description(tr("Type these in the search field. Spanish keywords work too: tipo:, en:, nombre:."))
        .build();
    for (example, description) in tips {
        let row = adw::ActionRow::builder()
            .title(format!("<tt>{}</tt>", glib::markup_escape_text(example)))
            .subtitle(glib::markup_escape_text(&description))
            .activatable(true)
            .tooltip_text(tr("Try it"))
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        row.connect_activated(glib::clone!(#[weak] dialog, #[strong] try_example, move |_| {
            dialog.close();
            try_example(example);
        }));
        group.add(&row);
    }
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    dialog.set_child(Some(&view));
    dialog.present(Some(parent));
}
