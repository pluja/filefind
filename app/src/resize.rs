//! Drag handles that let the user resize a split view's sidebar.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;

pub const MIN_WIDTH: f64 = 200.0;
pub const MAX_WIDTH: f64 = 720.0;

/// Fixes the sidebar of `split` at `width`.
pub fn set_width(split: &adw::OverlaySplitView, width: f64) {
    let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
    split.set_min_sidebar_width(width);
    split.set_max_sidebar_width(width);
}

/// Wraps `sidebar` so its inner edge can be dragged; `on_resized` gets the final width.
pub fn resizable(split: &adw::OverlaySplitView, sidebar: &impl IsA<gtk::Widget>, on_resized: impl Fn(f64) + 'static) -> gtk::Overlay {
    // The inner edge faces the content: the right edge of a start sidebar, and vice versa.
    let at_end = split.sidebar_position() == gtk::PackType::Start;
    let handle = gtk::Box::builder()
        .width_request(6)
        .halign(if at_end { gtk::Align::End } else { gtk::Align::Start })
        .valign(gtk::Align::Fill)
        .cursor(&gtk::gdk::Cursor::from_name("col-resize", None).expect("standard cursor"))
        .build();
    handle.add_css_class("resize-handle");

    // (width, pointer x in window coordinates) when the drag began. The handle itself moves
    // while dragging, so offsets relative to it would drift.
    let start = Rc::new(Cell::new((0.0, 0.0)));
    let drag = gtk::GestureDrag::new();
    let (split_ref, begin) = (split.clone(), start.clone());
    drag.connect_drag_begin(move |gesture, _, _| {
        if let Some(x) = pointer_x(gesture) {
            begin.set((split_ref.min_sidebar_width(), x));
        }
    });
    let split_ref = split.clone();
    drag.connect_drag_update(move |gesture, _, _| {
        let Some(x) = pointer_x(gesture) else { return };
        let (width, x0) = start.get();
        let dx = if at_end { x - x0 } else { x0 - x };
        set_width(&split_ref, width + dx);
    });
    let split_ref = split.clone();
    drag.connect_drag_end(move |_, _, _| on_resized(split_ref.min_sidebar_width()));
    handle.add_controller(drag);

    let overlay = gtk::Overlay::builder().child(sidebar).build();
    overlay.add_overlay(&handle);
    overlay
}

fn pointer_x(gesture: &gtk::GestureDrag) -> Option<f64> {
    let widget = gesture.widget()?;
    let (x, y) = gesture.point(None)?;
    let root = widget.root()?;
    widget.compute_point(&root, &gtk::graphene::Point::new(x as f32, y as f32)).map(|p| p.x() as f64)
}
