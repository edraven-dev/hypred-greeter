//! Session picker: a DropDown over the discovered .desktop sessions,
//! preselected from the per-user last-session cache. X11 entries are
//! suffixed so identical names stay distinguishable.

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::sessions::Kind;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct SessionDef;

impl WidgetDef for SessionDef {
    fn kind(&self) -> &'static str {
        "session"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let _ = node;
        let shared = &ctx.app.shared;
        if shared.sessions.is_empty() {
            return Err(WidgetError::Other(
                "no sessions found in wayland-sessions/xsessions".into(),
            ));
        }

        let names: Vec<String> = shared
            .sessions
            .iter()
            .map(|s| match s.kind {
                Kind::Wayland => s.name.clone(),
                Kind::X11 => format!("{} (X11)", s.name),
            })
            .collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let dropdown = gtk::DropDown::from_strings(&name_refs);
        dropdown.set_selected(shared.selected_session.get() as u32);

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
}
