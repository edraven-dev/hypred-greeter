use gtk4::Align;
use std::cell::RefCell;
use std::collections::HashSet;

use crate::widgets::WidgetError;

pub struct Node {
    pub kind: String,
    pub name: Option<String>,
    pub classes: Vec<String>,
    pub common: Common,
    pub props: Props,
    pub children: Vec<Node>,
    pub path: String,
}

/// Pixels, or a share of the window ("25%") — GTK has no relative sizes,
/// the builder keeps such a request in step with the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    Px(i32),
    Percent(f64),
}

impl Size {
    pub fn parse(value: &toml::Value) -> Option<Self> {
        if let Some(n) = value.as_integer() {
            return i32::try_from(n).ok().map(Size::Px);
        }
        let share: f64 = value.as_str()?.strip_suffix('%')?.trim().parse().ok()?;
        (share > 0.0 && share <= 100.0).then_some(Size::Percent(share))
    }

    /// The request in pixels, given the window's extent along that axis.
    pub fn request(self, window: i32) -> i32 {
        match self {
            Size::Px(n) => n,
            Size::Percent(share) => (f64::from(window) * share / 100.0).round() as i32,
        }
    }
}

#[derive(Default)]
pub struct Common {
    pub halign: Option<Align>,
    pub valign: Option<Align>,
    pub hexpand: Option<bool>,
    pub vexpand: Option<bool>,
    /// [top, right, bottom, left]
    pub margin: Option<[i32; 4]>,
    pub width: Option<Size>,
    pub height: Option<Size>,
    pub visible: Option<bool>,
    /// Grab focus once mapped.
    pub focus: Option<bool>,
    pub focusable: Option<bool>,
}

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

    /// `xalign = 1` is as valid as `xalign = 0.5`.
    pub fn float(&self, key: &str) -> Result<Option<f64>, WidgetError> {
        match self.raw(key) {
            None => Ok(None),
            Some(toml::Value::Float(f)) => Ok(Some(*f)),
            Some(toml::Value::Integer(n)) => Ok(Some(*n as f64)),
            Some(other) => Err(WidgetError::bad_prop(key, "a number", other)),
        }
    }

    pub fn str_list(&self, key: &str) -> Result<Option<Vec<String>>, WidgetError> {
        let Some(value) = self.raw(key) else { return Ok(None) };
        let items = value
            .as_array()
            .and_then(|items| items.iter().map(|v| v.as_str().map(str::to_string)).collect());
        items.map(Some).ok_or_else(|| WidgetError::bad_prop(key, "an array of strings", value))
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn props(toml: &str) -> Props {
        Props::new(toml.parse().unwrap())
    }

    #[test]
    fn accessors_read_and_default() {
        let p = props("format = \"%H\"\nspacing = 4\nwrap = true\n");
        assert_eq!(p.str("format").unwrap().as_deref(), Some("%H"));
        assert_eq!(p.str_or("missing", "x").unwrap(), "x");
        assert_eq!(p.int("spacing").unwrap(), Some(4));
        assert_eq!(p.bool("wrap").unwrap(), Some(true));
    }

    #[test]
    fn type_mismatch_errors_but_still_consumes_the_key() {
        let p = props("col = \"two\"\n");
        assert!(p.int("col").is_err());
        assert!(p.unconsumed().is_empty());
    }

    #[test]
    fn floats_take_integers_and_lists_take_only_strings() {
        let p = props(
            "xalign = 0.5\nwhole = 1\nword = \"x\"\ncommand = [\"sh\", \"-c\", \"id\"]\nmixed = [\"a\", 1]\n",
        );
        assert_eq!(p.float("xalign").unwrap(), Some(0.5));
        assert_eq!(p.float("whole").unwrap(), Some(1.0));
        assert_eq!(p.float("missing").unwrap(), None);
        assert!(p.float("word").is_err());
        assert_eq!(p.str_list("command").unwrap().unwrap(), ["sh", "-c", "id"]);
        assert_eq!(p.str_list("missing").unwrap(), None);
        let err = p.str_list("mixed").unwrap_err().to_string();
        assert!(err.contains("an array of strings"), "{err}");
        assert!(p.unconsumed().is_empty());
    }

    #[test]
    fn unconsumed_lists_exactly_the_untouched_keys() {
        let p = props("read = 1\nnever-read = 2\n");
        let _ = p.int("read");
        assert_eq!(p.unconsumed(), ["never-read"]);
    }
}
