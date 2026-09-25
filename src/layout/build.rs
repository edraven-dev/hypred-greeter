use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::node::Size;
use crate::layout::Node;
use crate::ui::ctx::BuildCtx;
use crate::widgets::WidgetError;

pub fn build_node(ctx: &BuildCtx, node: &Node) -> gtk::Widget {
    let built = match ctx.registry.get(&node.kind) {
        Some(def) => {
            if !def.is_container() && !node.children.is_empty() {
                ctx.problem(format!(
                    "{} ({}): children are ignored — not a container",
                    node.path, node.kind
                ));
            }
            def.build(ctx, node)
        }
        None => Err(WidgetError::Other(format!(
            "unknown widget (available: {})",
            ctx.registry.kinds().join(", ")
        ))),
    };

    let widget = match built {
        Ok(widget) => {
            // Typo sweep only after a full build: a failed widget never
            // read its remaining props — flagging those as "unknown" would
            // bury the real problem in false ones.
            for key in node.props.unconsumed() {
                ctx.problem(format!("{} ({}): unknown property `{key}`", node.path, node.kind));
            }
            widget
        }
        Err(err) => placeholder(ctx, node, &err),
    };

    apply_common(&widget, node);
    widget
}

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
    request_size(widget, common.width, common.height, (0, 0));
    let relative = |size| matches!(size, Some(Size::Percent(_)));
    if relative(common.width) || relative(common.height) {
        follow_window(widget, common.width, common.height);
    }
    if let Some(visible) = common.visible {
        widget.set_visible(visible);
    }
    if let Some(focusable) = common.focusable {
        widget.set_focusable(focusable);
        if !focusable {
            widget.set_can_focus(false);
        }
    }
    if common.focus == Some(true) {
        widget.connect_map(|widget| {
            widget.grab_focus();
        });
    }
}

fn request_size(
    widget: &gtk::Widget,
    width: Option<Size>,
    height: Option<Size>,
    window: (i32, i32),
) {
    let px = |size: Option<Size>, of: i32| match size {
        // Until the window has a size a share of it requests nothing.
        Some(Size::Percent(_)) if of <= 0 => -1,
        Some(size) => size.request(of),
        None => -1,
    };
    widget.set_size_request(px(width, window.0), px(height, window.1));
}

/// Keeps a "25%" request in step with the window: GTK has no relative
/// sizes, so the toplevel surface's size is followed instead.
fn follow_window(widget: &gtk::Widget, width: Option<Size>, height: Option<Size>) {
    widget.connect_realize(move |widget| {
        let Some(surface) = widget.native().and_then(|native| native.surface()) else { return };
        let weak = widget.downgrade();
        let apply = move |surface: &gtk::gdk::Surface| {
            if let Some(widget) = weak.upgrade() {
                request_size(&widget, width, height, (surface.width(), surface.height()));
            }
        };
        apply(&surface);
        surface.connect_width_notify(apply.clone());
        surface.connect_height_notify(apply);
    });
}
