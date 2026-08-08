//! Parsed layout tree. Parsing is forgiving: every recoverable problem is
//! recorded as a string and the affected prop/subtree is skipped — a typo in
//! one widget must never take down the whole layout.

use gtk4::Align;
use std::cell::RefCell;
use std::collections::HashSet;

use crate::widgets::WidgetError;

pub struct Node {
    /// The `widget = "..."` kind, resolved against the registry.
    pub kind: String,
    /// CSS name (`#name`); defaults to `hg-<kind>` when absent.
    pub name: Option<String>,
    /// Extra CSS classes on top of the automatic `hg-<kind>`.
    pub classes: Vec<String>,
    pub common: Common,
    pub props: Props,
    pub children: Vec<Node>,
    /// Dotted position in the layout file, for error messages.
    pub path: String,
}

/// Properties every widget shares, applied by the build walk.
#[derive(Default)]
pub struct Common {
    pub halign: Option<Align>,
    pub valign: Option<Align>,
    pub hexpand: Option<bool>,
    pub vexpand: Option<bool>,
    /// [top, right, bottom, left]
    pub margin: Option<[i32; 4]>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub visible: Option<bool>,
}

/// Widget-specific properties: what's left of the node's table after the
/// common keys. Accessors track consumption so the build walk can flag typos.
pub struct Props {
    table: toml::Table,
    consumed: RefCell<HashSet<String>>,
}

impl Props {
    pub fn new(table: toml::Table) -> Self {
        Self { table, consumed: RefCell::new(HashSet::new()) }
    }

    fn raw(&self, key: &str) -> Option<&toml::Value> {
        let value = self.table.get(key);
        if value.is_some() {
            self.consumed.borrow_mut().insert(key.to_string());
        }
        value
    }

    pub fn str(&self, key: &str) -> Result<Option<String>, WidgetError> {
        match self.raw(key) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Ok(Some(s.clone())),
            Some(other) => Err(WidgetError::bad_prop(key, "a string", other)),
        }
    }

    pub fn str_or(&self, key: &str, default: &str) -> Result<String, WidgetError> {
        Ok(self.str(key)?.unwrap_or_else(|| default.to_string()))
    }

    pub fn int(&self, key: &str) -> Result<Option<i64>, WidgetError> {
        match self.raw(key) {
            None => Ok(None),
            Some(toml::Value::Integer(n)) => Ok(Some(*n)),
            Some(other) => Err(WidgetError::bad_prop(key, "an integer", other)),
        }
    }

    pub fn bool(&self, key: &str) -> Result<Option<bool>, WidgetError> {
        match self.raw(key) {
            None => Ok(None),
            Some(toml::Value::Boolean(b)) => Ok(Some(*b)),
            Some(other) => Err(WidgetError::bad_prop(key, "a boolean", other)),
        }
    }

    /// Keys the widget never looked at — almost always typos.
    pub fn unconsumed(&self) -> Vec<String> {
        let consumed = self.consumed.borrow();
        self.table.keys().filter(|k| !consumed.contains(*k)).cloned().collect()
    }
}

pub fn parse_align(value: &str) -> Option<Align> {
    Some(match value {
        "start" => Align::Start,
        "center" => Align::Center,
        "end" => Align::End,
        "fill" => Align::Fill,
        _ => return None,
    })
}

/// `anchor` is sugar for halign+valign, meaningful mostly for overlay
/// children: "center", "top", "bottom-right", "left", "fill", ...
pub fn parse_anchor(value: &str) -> Option<(Align, Align)> {
    Some(match value {
        "center" => (Align::Center, Align::Center),
        "fill" => (Align::Fill, Align::Fill),
        "top" => (Align::Center, Align::Start),
        "bottom" => (Align::Center, Align::End),
        "left" => (Align::Start, Align::Center),
        "right" => (Align::End, Align::Center),
        "top-left" => (Align::Start, Align::Start),
        "top-right" => (Align::End, Align::Start),
        "bottom-left" => (Align::Start, Align::End),
        "bottom-right" => (Align::End, Align::End),
        _ => return None,
    })
}
