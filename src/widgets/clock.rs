use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;

use crate::layout::Node;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{apply_text_props, WidgetDef, WidgetError};

pub struct ClockDef;

pub fn now(format: &str) -> Option<glib::GString> {
    glib::DateTime::now_local().ok()?.format(format).ok()
}

/// A strftime format cut into its conversions (`%H`, `%-d`, `%:z`) and the
/// text between them, `true` marking a conversion.
fn pieces(format: &str) -> Vec<(bool, &str)> {
    let mut pieces = Vec::new();
    let mut rest = format;
    while !rest.is_empty() {
        let (conversion, end) = match rest.strip_prefix('%') {
            Some(spec) => {
                let flags = spec.len()
                    - spec.trim_start_matches(['-', '_', '0', '^', '#', 'E', 'O', ':']).len();
                (true, 1 + flags + spec[flags..].chars().next().map_or(0, char::len_utf8))
            }
            None => (false, rest.find('%').unwrap_or(rest.len())),
        };
        pieces.push((conversion, &rest[..end]));
        rest = &rest[end..];
    }
    pieces
}

/// Under markup the format's own text is markup (`<b>%H</b>`) and what a
/// conversion puts in is escaped, as a label's `{time:…}` is.
fn render(format: &str, markup: bool, convert: impl Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    for (conversion, piece) in pieces(format) {
        match conversion {
            true if markup => out.push_str(&glib::markup_escape_text(&convert(piece)?)),
            true => out.push_str(&convert(piece)?),
            false => out.push_str(piece),
        }
    }
    Some(out)
}

fn text(format: &str, markup: bool) -> Option<String> {
    let now = glib::DateTime::now_local().ok()?;
    render(format, markup, |conversion| now.format(conversion).ok().map(Into::into))
}

fn check_markup(format: &str) -> Result<(), glib::Error> {
    let shown = render(format, true, |_| Some("x".into())).unwrap_or_default();
    gtk::pango::parse_markup(&shown, '\0').map(drop)
}

impl WidgetDef for ClockDef {
    fn kind(&self) -> &'static str {
        "clock"
    }

    fn build(&self, _ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let format = node.props.str_or("format", "%H:%M")?;
        if now(&format).is_none() {
            return Err(WidgetError::Other(format!("`format`: invalid strftime `{format}`")));
        }

        let label = gtk::Label::new(None);
        apply_text_props(&label, node)?;
        let markup = label.uses_markup();
        if markup {
            if let Err(err) = check_markup(&format) {
                return Err(WidgetError::Other(format!(
                    "`format` is not valid Pango markup: {err}"
                )));
            }
        }
        let show = move |label: &gtk::Label| {
            if let Some(text) = text(&format, markup) {
                label.set_label(&text);
            }
        };
        show(&label);
        let weak = label.downgrade();
        glib::timeout_add_seconds_local(1, move || match weak.upgrade() {
            Some(label) => {
                show(&label);
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        Ok(label.upcast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_format_is_cut_at_its_conversions() {
        let cut = pieces("<b>%H</b>:%-M %:::z%%x %Ey é%");
        let expected = [
            (false, "<b>"),
            (true, "%H"),
            (false, "</b>:"),
            (true, "%-M"),
            (false, " "),
            (true, "%:::z"),
            (true, "%%"),
            (false, "x "),
            (true, "%Ey"),
            (false, " é"),
            (true, "%"),
        ];
        assert_eq!(cut, expected);
        assert_eq!(pieces("%é"), [(true, "%é")]);
        assert!(pieces("").is_empty());
    }

    #[test]
    fn piece_by_piece_is_what_the_whole_format_gives() {
        let at = glib::DateTime::from_utc(2026, 10, 17, 14, 5, 9.0).unwrap();
        let convert = |conversion: &str| at.format(conversion).ok().map(String::from);
        for format in ["%H:%M", "%A %e %B  %H:%M", "%-d.%m. %:z %%", "x%", "%a, %Y-%m-%d"] {
            let whole = at.format(format).unwrap();
            assert_eq!(render(format, false, convert).as_deref(), Some(whole.as_str()), "{format}");
        }
        assert_eq!(render("%H %Q", false, convert), None);
    }

    #[test]
    fn under_markup_only_what_conversions_put_in_is_escaped() {
        let convert =
            |conversion: &str| Some(if conversion == "%p" { "a&b<".into() } else { "14".into() });
        assert_eq!(render("<b>%H</b> %p", true, convert).unwrap(), "<b>14</b> a&amp;b&lt;");
        assert_eq!(render("<b>%H</b> %p", false, convert).unwrap(), "<b>14</b> a&b<");
    }

    #[test]
    fn the_markup_check_sees_what_gtk_will_parse() {
        for ok in ["<b>%H</b>:%M", "%H &amp; %M", "%p", "%%"] {
            assert!(check_markup(ok).is_ok(), "{ok}");
        }
        for bad in ["%H & %M", "<b>%H", "%H < %M"] {
            assert!(check_markup(bad).is_err(), "{bad}");
        }
    }
}
