use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct ClockDef;

fn now(format: &str) -> Option<glib::GString> {
    glib::DateTime::now_local().ok()?.format(format).ok()
}

impl WidgetDef for ClockDef {
    fn kind(&self) -> &'static str {
        "clock"
    }

    fn build(&self, _ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let format = node.props.str_or("format", "%H:%M")?;
        let Some(text) = now(&format) else {
            return Err(WidgetError::Other(format!("`format`: invalid strftime `{format}`")));
        };

        let label = gtk::Label::new(Some(&text));
        let weak = label.downgrade();
        glib::timeout_add_seconds_local(1, move || match weak.upgrade() {
            Some(label) => {
                if let Some(text) = now(&format) {
                    label.set_label(&text);
                }
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        Ok(label.upcast())
    }
}
