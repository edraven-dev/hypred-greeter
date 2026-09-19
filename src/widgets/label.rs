use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct LabelDef;

impl WidgetDef for LabelDef {
    fn kind(&self) -> &'static str {
        "label"
    }

    fn build(&self, _ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let label = gtk::Label::builder()
            .label(node.props.str_or("text", "")?)
            .wrap(node.props.bool("wrap")?.unwrap_or(false))
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        // Without it a wrapping label asks for its one-line width and
        // stretches its container instead of wrapping.
        if let Some(chars) = node.props.int("max-width-chars")? {
            label.set_max_width_chars(chars as i32);
        }
        Ok(label.upcast())
    }
}
