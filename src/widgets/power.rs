//! Reboot / power-off buttons running the [commands] argv arrays. In demo
//! they announce instead of executing. Buttons carry stable names
//! (#hg-power-reboot, #hg-power-poweroff) so themes can style or hide each.

use gtk4 as gtk;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct PowerDef;

impl WidgetDef for PowerDef {
    fn kind(&self) -> &'static str {
        "power"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let row = gtk::Box::new(
            gtk::Orientation::Horizontal,
            node.props.int("spacing")?.unwrap_or(8) as i32,
        );
        for (id, label_key, default_label, command) in [
            ("hg-power-reboot", "reboot-label", "Reboot", ctx.config.commands.reboot.clone()),
            (
                "hg-power-poweroff",
                "poweroff-label",
                "Power Off",
                ctx.config.commands.poweroff.clone(),
            ),
        ] {
            let button = gtk::Button::with_label(&node.props.str_or(label_key, default_label)?);
            button.set_widget_name(id);
            let app = ctx.app.clone();
            let demo = ctx.demo;
            button.connect_clicked(move |_| {
                if demo {
                    app.emit(&UiEvent::Info(format!("demo: would run {}", command.join(" "))));
                    return;
                }
                let Some((program, args)) = command.split_first() else {
                    app.emit(&UiEvent::AuthError("power: empty command configured".into()));
                    return;
                };
                if let Err(err) = std::process::Command::new(program).args(args).spawn() {
                    app.emit(&UiEvent::AuthError(format!("power: {program}: {err}")));
                }
            });
            row.append(&button);
        }
        Ok(row.upcast())
    }
}
