mod background;
mod clock;
mod containers;
mod label;
mod message;
mod password;
mod power;
mod session;
mod username;

use gtk4 as gtk;
use std::collections::HashMap;
use std::fmt;

use crate::layout::Node;
use crate::ui::ctx::BuildCtx;

pub trait WidgetDef {
    fn kind(&self) -> &'static str;

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError>;

    fn is_container(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub enum WidgetError {
    BadProp { key: String, expected: &'static str, found: &'static str },
    Other(String),
}

impl WidgetError {
    pub fn bad_prop(key: &str, expected: &'static str, found: &toml::Value) -> Self {
        Self::BadProp { key: key.to_string(), expected, found: found.type_str() }
    }
}

impl fmt::Display for WidgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadProp { key, expected, found } => {
                write!(f, "`{key}` expects {expected}, found {found}")
            }
            Self::Other(message) => f.write_str(message),
        }
    }
}

pub struct Registry(HashMap<&'static str, Box<dyn WidgetDef>>);

impl Registry {
    pub fn builtin() -> Self {
        let mut registry = Self(HashMap::new());
        registry.register(Box::new(containers::BoxDef));
        registry.register(Box::new(containers::OverlayDef));
        registry.register(Box::new(containers::GridDef));
        registry.register(Box::new(label::LabelDef));
        registry.register(Box::new(background::BackgroundDef));
        registry.register(Box::new(clock::ClockDef));
        registry.register(Box::new(message::MessageDef));
        registry.register(Box::new(username::UsernameDef));
        registry.register(Box::new(password::PasswordDef));
        registry.register(Box::new(session::SessionDef));
        registry.register(Box::new(power::PowerDef));
        registry
    }

    pub fn register(&mut self, def: Box<dyn WidgetDef>) {
        let kind = def.kind();
        if self.0.insert(kind, def).is_some() {
            crate::log::warn_!("widget `{kind}` registered twice; last wins");
        }
    }

    pub fn get(&self, kind: &str) -> Option<&dyn WidgetDef> {
        self.0.get(kind).map(Box::as_ref)
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        let mut kinds: Vec<_> = self.0.keys().copied().collect();
        kinds.sort_unstable();
        kinds
    }
}
