use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::node::Common;
use crate::layout::{Node, Props};
use crate::ui::ctx::BuildCtx;
use crate::widgets::{containers, WidgetDef, WidgetError};

/// Sugar: a box of two `button`s, `#hg-power-reboot` and
/// `#hg-power-poweroff`, running the `reboot` and `poweroff` commands.
pub struct PowerDef;

impl WidgetDef for PowerDef {
    fn kind(&self) -> &'static str {
        "power"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let row = gtk::Box::new(
            containers::parse_orientation(&node.props, "horizontal")?,
            node.props.int("spacing")?.unwrap_or(0) as i32,
        );
        for (action, label_key, default) in
            [("reboot", "reboot-label", "Reboot"), ("poweroff", "poweroff-label", "Power Off")]
        {
            let label = node.props.str_or(label_key, default)?;
            row.append(&ctx.build_child(&button(node, action, label)));
        }
        Ok(row.upcast())
    }
}

fn button(parent: &Node, action: &str, label: String) -> Node {
    let mut props = toml::Table::new();
    props.insert("label".into(), label.into());
    props.insert("action".into(), action.into());
    Node {
        kind: "button".into(),
        name: Some(format!("hg-power-{action}")),
        classes: Vec::new(),
        common: Common::default(),
        props: Props::new(props),
        children: Vec::new(),
        path: format!("{}.{action}", parent.path),
    }
}
