//! The build walk: `Node` tree → GTK widget tree via the registry. Every
//! failure is localized — an unknown widget or bad prop becomes an inline
//! placeholder plus a recorded problem, and the rest of the tree still builds.

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::ctx::BuildCtx;
use crate::widgets::WidgetError;

pub fn build_node(ctx: &BuildCtx, node: &Node) -> gtk::Widget {
    let widget = match ctx.registry.get(&node.kind) {
        Some(def) => {
            if !def.is_container() && !node.children.is_empty() {
                ctx.problem(format!(
                    "{} ({}): children are ignored — not a container",
                    node.path, node.kind
                ));
            }
            match def.build(ctx, node) {
                Ok(widget) => widget,
                Err(err) => placeholder(ctx, node, &err),
            }
        }
        None => placeholder(
            ctx,
            node,
            &WidgetError::Other(format!(
                "unknown widget (available: {})",
                ctx.registry.kinds().join(", ")
            )),
        ),
    };

    apply_common(&widget, node);
    for key in node.props.unconsumed() {
        ctx.problem(format!("{} ({}): unknown property `{key}`", node.path, node.kind));
    }
    widget
}

/// Visible, styleable stand-in for a widget that failed to build. Keeps the
/// layout's shape so the problem is obvious and the rest still works.
fn placeholder(ctx: &BuildCtx, node: &Node, err: &WidgetError) -> gtk::Widget {
    ctx.problem(format!("{} ({}): {err}", node.path, node.kind));
    let label = gtk::Label::builder()
        .label(format!("⚠ {}", node.kind))
        .tooltip_text(err.to_string())
        .build();
    label.add_css_class("hg-error");
    label.upcast()
}

fn apply_common(widget: &gtk::Widget, node: &Node) {
    widget.add_css_class(&format!("hg-{}", node.kind));
    widget.set_widget_name(node.name.as_deref().unwrap_or(&format!("hg-{}", node.kind)));
    for class in &node.classes {
        widget.add_css_class(class);
    }

    let common = &node.common;
    if let Some(halign) = common.halign {
        widget.set_halign(halign);
    }
    if let Some(valign) = common.valign {
        widget.set_valign(valign);
    }
    if let Some(hexpand) = common.hexpand {
        widget.set_hexpand(hexpand);
    }
    if let Some(vexpand) = common.vexpand {
        widget.set_vexpand(vexpand);
    }
    if let Some([top, right, bottom, left]) = common.margin {
        widget.set_margin_top(top);
        widget.set_margin_end(right);
        widget.set_margin_bottom(bottom);
        widget.set_margin_start(left);
    }
    widget.set_size_request(common.width.unwrap_or(-1), common.height.unwrap_or(-1));
    if let Some(visible) = common.visible {
        widget.set_visible(visible);
    }
}
