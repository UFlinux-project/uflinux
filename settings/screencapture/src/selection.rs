//! Area selection in output-local logical coordinates.
use std::cell::Cell;
use std::rc::Rc;

use gtk::gdk;
use gtk::prelude::*;

use crate::backend::Region;

const MIN_SIZE: i32 = 8;

pub fn area(
    output: gdk::Rectangle,
    on_change: impl Fn(Option<Region>) + 'static,
) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_cursor_from_name(Some("crosshair"));
    let selection = Rc::new(Cell::new(None::<(i32, i32, i32, i32)>));
    let state = selection.clone();
    area.set_draw_func(move |_, cr, width, height| {
        cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
        cr.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
        if let Some((x, y, w, h)) = state.get() {
            cr.rectangle(x.into(), y.into(), w.into(), h.into());
        }
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.38);
        let _ = cr.fill();
        if let Some((x, y, w, h)) = state.get() {
            cr.rectangle(x.into(), y.into(), w.into(), h.into());
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
            cr.set_line_width(2.0);
            let _ = cr.stroke();
        }
    });

    let drag = gtk::GestureDrag::new();
    let update = {
        let area = area.downgrade();
        move |gesture: &gtk::GestureDrag, dx: f64, dy: f64, finished: bool| {
            let (Some(area), Some((x, y))) = (area.upgrade(), gesture.start_point()) else {
                return;
            };
            let edges = |start: f64, end: f64, bounds: i32| {
                let clamp = |value: f64| (value.round() as i32).clamp(0, bounds);
                (clamp(start.min(end)), clamp(start.max(end)))
            };
            let (x, right) = edges(x, x + dx, output.width());
            let (y, bottom) = edges(y, y + dy, output.height());
            let (width, height) = (right - x, bottom - y);
            let valid = width >= MIN_SIZE && height >= MIN_SIZE;
            selection.set((valid || !finished).then_some((x, y, width, height)));
            on_change((finished && valid).then_some(Region {
                output: output.clone(),
                x,
                y,
                width,
                height,
            }));
            area.queue_draw();
        }
    };
    let update = Rc::new(update);
    let begin = update.clone();
    drag.connect_drag_begin(move |gesture, _, _| begin(gesture, 0.0, 0.0, false));
    let motion = update.clone();
    drag.connect_drag_update(move |gesture, x, y| motion(gesture, x, y, false));
    drag.connect_drag_end(move |gesture, x, y| update(gesture, x, y, true));
    area.add_controller(drag);
    area
}
