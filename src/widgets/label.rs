//! Static text. `text` is the content; `wrap` opts into line wrapping.

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
            .build();
        Ok(label.upcast())
    }
}
