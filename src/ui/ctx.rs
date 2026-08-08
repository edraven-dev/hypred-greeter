//! Everything a widget may need while building. New capabilities land here
//! (not on the trait), so existing widgets never break when one is added.

use gtk4 as gtk;
use std::cell::RefCell;
use std::rc::Rc;

use crate::config::Config;
use crate::layout::{build, Node};
use crate::ui::bus::Bus;
use crate::widgets::Registry;

pub struct BuildCtx<'a> {
    pub config: &'a Config,
    pub registry: &'a Registry,
    /// Widgets clone this into closures to hear auth/system events.
    pub bus: Rc<Bus>,
    /// True in --demo: widgets with side effects (power, session start)
    /// must print instead of executing.
    pub demo: bool,
    problems: RefCell<Vec<String>>,
}

impl<'a> BuildCtx<'a> {
    pub fn new(config: &'a Config, registry: &'a Registry, bus: Rc<Bus>, demo: bool) -> Self {
        Self { config, registry, bus, demo, problems: RefCell::new(Vec::new()) }
    }

    /// Containers recurse through this so every node gets the same
    /// registry-lookup / common-props / error-placeholder treatment.
    pub fn build_child(&self, node: &Node) -> gtk::Widget {
        build::build_node(self, node)
    }

    pub fn problem(&self, message: String) {
        crate::log::error!("{message}");
        self.problems.borrow_mut().push(message);
    }

    pub fn take_problems(&self) -> Vec<String> {
        self.problems.take()
    }
}
