use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{containers, WidgetDef, WidgetError};

pub struct SessionDef;

impl WidgetDef for SessionDef {
    fn kind(&self) -> &'static str {
        "session"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let shared = &ctx.app.shared;
        if shared.sessions.is_empty() {
            return Err(WidgetError::Other(
                "no sessions found in wayland-sessions/xsessions".into(),
            ));
        }
        let names: Vec<String> = shared.sessions.iter().map(|s| s.name.clone()).collect();
        let selected = shared.selected_session.get();
        match node.props.str_or("style", "dropdown")?.as_str() {
            "dropdown" => dropdown(ctx, &names, selected),
            "buttons" => buttons(ctx, node, &names, selected),
            "cycle" => cycle(ctx, names, selected),
            other => Err(WidgetError::Other(format!(
                "`style`: unknown style `{other}` (dropdown, buttons, cycle)"
            ))),
        }
    }
}

fn dropdown(ctx: &BuildCtx, names: &[String], selected: usize) -> Result<gtk::Widget, WidgetError> {
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let dropdown = gtk::DropDown::from_strings(&name_refs);
    dropdown.set_selected(selected as u32);

    let app = ctx.app.clone();
    dropdown.connect_selected_notify(move |dropdown| {
        app.select_session(dropdown.selected() as usize);
    });

    let weak = dropdown.downgrade();
    ctx.bus.subscribe(move |event| {
        if let UiEvent::SessionChanged(index) = event {
            if let Some(dropdown) = weak.upgrade() {
                dropdown.set_selected(*index as u32);
            }
        }
    });
    Ok(dropdown.upcast())
}

/// Linked toggle buttons, the selected one `:checked`.
fn buttons(
    ctx: &BuildCtx,
    node: &Node,
    names: &[String],
    selected: usize,
) -> Result<gtk::Widget, WidgetError> {
    let row = gtk::Box::new(
        containers::parse_orientation(&node.props, "horizontal")?,
        node.props.int("spacing")?.unwrap_or(0) as i32,
    );
    row.add_css_class("linked");
    let mut items: Vec<gtk::ToggleButton> = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let item = gtk::ToggleButton::with_label(name);
        item.add_css_class("hg-session-item");
        item.set_group(items.first());
        item.set_active(index == selected);
        let app = ctx.app.clone();
        item.connect_toggled(move |item| {
            if item.is_active() {
                app.select_session(index);
            }
        });
        row.append(&item);
        items.push(item);
    }

    let weak: Vec<_> = items.iter().map(|item| item.downgrade()).collect();
    ctx.bus.subscribe(move |event| {
        if let UiEvent::SessionChanged(index) = event {
            if let Some(item) = weak.get(*index).and_then(|item| item.upgrade()) {
                item.set_active(true);
            }
        }
    });
    Ok(row.upcast())
}

/// One button showing the selection; a click moves on to the next.
fn cycle(ctx: &BuildCtx, names: Vec<String>, selected: usize) -> Result<gtk::Widget, WidgetError> {
    let button = gtk::Button::with_label(&names[selected]);
    let app = ctx.app.clone();
    button.connect_clicked(move |_| app.step_session(1));

    let weak = button.downgrade();
    ctx.bus.subscribe(move |event| {
        if let UiEvent::SessionChanged(index) = event {
            if let (Some(button), Some(name)) = (weak.upgrade(), names.get(*index)) {
                button.set_label(name);
            }
        }
    });
    Ok(button.upcast())
}
