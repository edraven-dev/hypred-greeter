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
            let cell = (|| -> Result<(i32, i32, i32, i32), WidgetError> {
                Ok((
                    child.props.int("col")?.unwrap_or(0) as i32,
                    child.props.int("row")?.unwrap_or(i as i64) as i32,
                    child.props.int("col-span")?.unwrap_or(1) as i32,
                    child.props.int("row-span")?.unwrap_or(1) as i32,
                ))
            })();
            let (col, row, col_span, row_span) = cell.unwrap_or_else(|err| {
                ctx.problem(format!("{}: {err}", child.path));
                (0, i as i32, 1, 1)
            });
            grid.attach(&ctx.build_child(child), col, row, col_span, row_span);
        }
        Ok(grid.upcast())
    }
}
