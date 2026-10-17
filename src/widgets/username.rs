use gtk4 as gtk;
use gtk4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

use crate::layout::Node;
use crate::ui::bus::{FocusTarget, UiEvent};
use crate::ui::ctx::BuildCtx;
use crate::users;
use crate::widgets::{apply_entry_props, containers, WidgetDef, WidgetError};

pub struct UsernameDef;

enum Picker {
    Dropdown,
    Buttons(gtk::Orientation, i32),
}

impl WidgetDef for UsernameDef {
    fn kind(&self) -> &'static str {
        "username"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let widget = styled(ctx, node)?;
        // GTK marks neither a read-only entry nor its text node.
        if ctx.config.auth.fixed_user {
            widget.add_css_class("hg-username-fixed");
        }
        Ok(widget)
    }
}

fn styled(ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
    let picker = match node.props.str_or("style", "entry")?.as_str() {
        "entry" => return entry(ctx, node),
        "dropdown" => Picker::Dropdown,
        "buttons" => Picker::Buttons(
            containers::parse_orientation(&node.props, "horizontal")?,
            node.props.int("spacing")?.unwrap_or(0) as i32,
        ),
        other => {
            return Err(WidgetError::Other(format!(
                "`style`: unknown style `{other}` (entry, dropdown, buttons)"
            )))
        }
    };
    let users = node.props.str_list("users")?.unwrap_or_else(users::discover);
    let names = users::pickable(users, &ctx.app.username());
    if names.is_empty() {
        // Nothing else could name a user: login has to stay possible.
        ctx.problem(format!("{} (username): no user to pick from, an entry instead", node.path));
        return entry(ctx, node);
    }
    let widget = match picker {
        Picker::Dropdown => dropdown(ctx, names),
        Picker::Buttons(orientation, spacing) => buttons(ctx, names, orientation, spacing),
    };
    widget.set_sensitive(!ctx.config.auth.fixed_user);
    Ok(widget)
}

fn entry(ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
    let entry = gtk::Entry::builder()
        .placeholder_text(node.props.str_or("placeholder", "username")?)
        .text(ctx.app.username())
        .editable(!ctx.config.auth.fixed_user)
        .build();
    apply_entry_props(&entry, node)?;

    let app = ctx.app.clone();
    entry.connect_changed(move |entry| {
        let text = entry.text();
        if text.as_str() != app.username() {
            app.set_username(&text);
            app.username_edited();
        }
    });
    let app = ctx.app.clone();
    entry.connect_activate(move |_| {
        app.emit(&UiEvent::Focus(FocusTarget::Password));
        app.auth.start_eager(&app.username());
    });

    let weak = entry.downgrade();
    ctx.bus.subscribe(move |event| {
        let Some(entry) = weak.upgrade() else { return };
        match event {
            UiEvent::Focus(FocusTarget::Username) => {
                entry.grab_focus();
            }
            UiEvent::UsernameChanged(name) => {
                if entry.text().as_str() != name {
                    entry.set_text(name);
                }
            }
            UiEvent::Focus(FocusTarget::Password)
            | UiEvent::Prompt { .. }
            | UiEvent::Info(_)
            | UiEvent::PamError(_)
            | UiEvent::AuthError(_)
            | UiEvent::Busy(_)
            | UiEvent::SessionChanged(_)
            | UiEvent::Armed(_)
            | UiEvent::Starting => {}
        }
    });
    Ok(entry.upcast())
}

fn position(names: &[String], name: &str) -> Option<usize> {
    names.iter().position(|candidate| candidate == name)
}

/// GtkDropDown always has a row selected, so a name the list lacks (none
/// yet, or one typed into an entry) gets a last row of its own while it is
/// the one shown — picking a row that is already selected would go unseen.
struct Rows {
    names: Vec<String>,
    model: gtk::StringList,
    extra: Cell<bool>,
    following: Cell<bool>,
}

impl Rows {
    fn show(&self, dropdown: &gtk::DropDown, name: &str) {
        self.following.set(true);
        let after = self.names.len() as u32;
        match position(&self.names, name) {
            Some(index) => {
                dropdown.set_selected(index as u32);
                if self.extra.replace(false) {
                    self.model.remove(after);
                }
            }
            None => {
                if self.extra.replace(true) {
                    self.model.splice(after, 1, &[name]);
                } else {
                    self.model.append(name);
                }
                dropdown.set_selected(after);
            }
        }
        self.following.set(false);
    }
}

fn dropdown(ctx: &BuildCtx, names: Vec<String>) -> gtk::Widget {
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let model = gtk::StringList::new(&refs);
    let dropdown = gtk::DropDown::new(Some(model.clone()), gtk::Expression::NONE);
    let rows = Rc::new(Rows { names, model, extra: Cell::new(false), following: Cell::new(false) });
    rows.show(&dropdown, &ctx.app.username());

    let (app, picked) = (ctx.app.clone(), rows.clone());
    dropdown.connect_selected_notify(move |dropdown| {
        if picked.following.get() {
            return;
        }
        if let Some(name) = picked.names.get(dropdown.selected() as usize) {
            app.pick_user(name);
        }
    });

    let weak = dropdown.downgrade();
    ctx.bus.subscribe(move |event| {
        let Some(dropdown) = weak.upgrade() else { return };
        match event {
            UiEvent::UsernameChanged(name) => rows.show(&dropdown, name),
            UiEvent::Focus(FocusTarget::Username) => {
                dropdown.grab_focus();
            }
            UiEvent::Focus(FocusTarget::Password)
            | UiEvent::Prompt { .. }
            | UiEvent::Info(_)
            | UiEvent::PamError(_)
            | UiEvent::AuthError(_)
            | UiEvent::Busy(_)
            | UiEvent::SessionChanged(_)
            | UiEvent::Armed(_)
            | UiEvent::Starting => {}
        }
    });
    dropdown.upcast()
}

/// Linked toggle buttons, the selected one `:checked`.
fn buttons(
    ctx: &BuildCtx,
    names: Vec<String>,
    orientation: gtk::Orientation,
    spacing: i32,
) -> gtk::Widget {
    let row = gtk::Box::new(orientation, spacing);
    row.add_css_class("linked");
    let current = ctx.app.username();
    let mut items: Vec<gtk::ToggleButton> = Vec::new();
    for name in &names {
        let item = gtk::ToggleButton::with_label(name);
        item.add_css_class("hg-username-item");
        item.set_group(items.first());
        item.set_active(*name == current);
        let (app, name) = (ctx.app.clone(), name.clone());
        item.connect_toggled(move |item| {
            if item.is_active() {
                app.pick_user(&name);
            }
        });
        row.append(&item);
        items.push(item);
    }

    let weak: Vec<_> = items.iter().map(|item| item.downgrade()).collect();
    ctx.bus.subscribe(move |event| match event {
        UiEvent::UsernameChanged(name) => {
            let chosen = position(&names, name);
            for (index, item) in weak.iter().enumerate() {
                if let Some(item) = item.upgrade() {
                    item.set_active(Some(index) == chosen);
                }
            }
        }
        UiEvent::Focus(FocusTarget::Username) => {
            let items: Vec<gtk::ToggleButton> = weak.iter().filter_map(|i| i.upgrade()).collect();
            if let Some(item) = items.iter().find(|item| item.is_active()).or(items.first()) {
                item.grab_focus();
            }
        }
        UiEvent::Focus(FocusTarget::Password)
        | UiEvent::Prompt { .. }
        | UiEvent::Info(_)
        | UiEvent::PamError(_)
        | UiEvent::AuthError(_)
        | UiEvent::Busy(_)
        | UiEvent::SessionChanged(_)
        | UiEvent::Armed(_)
        | UiEvent::Starting => {}
    });
    row.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_not_offered_selects_nothing() {
        let names = vec!["fprint".to_string(), "touch".to_string()];
        assert_eq!(position(&names, "touch"), Some(1));
        assert_eq!(position(&names, "tou"), None);
        assert_eq!(position(&names, ""), None);
    }
}
