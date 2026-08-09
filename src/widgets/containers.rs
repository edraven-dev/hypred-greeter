//! Layout containers: `box`, `overlay`, `grid`. These are the only widgets
//! that consume `children`; everything else is a leaf.

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct BoxDef;

impl WidgetDef for BoxDef {
    fn kind(&self) -> &'static str {
        "box"
    }

    fn is_container(&self) -> bool {
        true
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let orientation = match node.props.str_or("orientation", "vertical")?.as_str() {
            "vertical" | "v" => gtk::Orientation::Vertical,
            "horizontal" | "h" => gtk::Orientation::Horizontal,
            _ => {
                return Err(WidgetError::Other(
                    "`orientation` must be \"vertical\" or \"horizontal\"".into(),
                ))
            }
        };
        let gtk_box = gtk::Box::builder()
            .orientation(orientation)
            .spacing(node.props.int("spacing")?.unwrap_or(0) as i32)
            .homogeneous(node.props.bool("homogeneous")?.unwrap_or(false))
            .build();
        for child in &node.children {
            gtk_box.append(&ctx.build_child(child));
        }
        Ok(gtk_box.upcast())
    }
}

pub struct OverlayDef;

impl WidgetDef for OverlayDef {
    fn kind(&self) -> &'static str {
        "overlay"
    }

    fn is_container(&self) -> bool {
        true
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        // First child is the measured base (where `background` belongs);
        // the rest float above it, positioned by halign/valign/anchor.
        let overlay = gtk::Overlay::new();
        let mut children = node.children.iter();
        if let Some(base) = children.next() {
            overlay.set_child(Some(&ctx.build_child(base)));
        }
        for child in children {
            overlay.add_overlay(&ctx.build_child(child));
        }
        Ok(overlay.upcast())
    }
}

pub struct GridDef;

impl WidgetDef for GridDef {
    fn kind(&self) -> &'static str {
        "grid"
    }

    fn is_container(&self) -> bool {
        true
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let grid = gtk::Grid::builder()
            .row_spacing(node.props.int("row-spacing")?.unwrap_or(0) as i32)
            .column_spacing(node.props.int("column-spacing")?.unwrap_or(0) as i32)
            .build();
        for (i, child) in node.children.iter().enumerate() {
            // Cell position lives on the child node but belongs to the grid;
            // read it before build_child so it doesn't count as unknown.
            // Each key is read independently: a bad `col` must not stop
            // `row` from being consumed (and later flagged as "unknown").
            let cell = |key: &str, default: i64| match child.props.int(key) {
                Ok(value) => value.unwrap_or(default) as i32,
                Err(err) => {
                    ctx.problem(format!("{}: {err}", child.path));
                    default as i32
                }
            };
            let (col, row) = (cell("col", 0), cell("row", i as i64));
            let (col_span, row_span) = (cell("col-span", 1), cell("row-span", 1));
            grid.attach(&ctx.build_child(child), col, row, col_span, row_span);
        }
        Ok(grid.upcast())
    }
}
