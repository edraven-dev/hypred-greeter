use gtk4 as gtk;
use gtk4::gdk;
use gtk4::prelude::*;
use std::path::{Path, PathBuf};

use crate::layout::Node;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct ImageDef;

/// GTK's own icon size, what a button's icon gets.
const ICON_SIZE: i32 = 16;

/// A theme icon by name, or an image file by absolute path.
pub fn load_icon(value: &str) -> Result<gtk::Image, WidgetError> {
    if value.starts_with('/') {
        return Ok(gtk::Image::from_paintable(Some(&paintable(Path::new(value), ICON_SIZE)?)));
    }
    let display = gdk::Display::default();
    if display.is_some_and(|display| !gtk::IconTheme::for_display(&display).has_icon(value)) {
        return Err(WidgetError::Other(format!("icon `{value}` is not in the icon theme")));
    }
    Ok(gtk::Image::from_icon_name(value))
}

/// An SVG is rendered at `size` by GTK's icon renderer (gdk-pixbuf no
/// longer loads SVGs); anything else is a texture at its own size.
fn paintable(path: &Path, size: i32) -> Result<gdk::Paintable, WidgetError> {
    if !path.is_file() {
        return Err(WidgetError::Other(format!("image {}: not a readable file", path.display())));
    }
    if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("svg")) {
        let file = gtk::gio::File::for_path(path);
        return Ok(gtk::IconPaintable::for_file(&file, size, 1).upcast());
    }
    gdk::Texture::from_filename(path)
        .map(Cast::upcast)
        .map_err(|err| WidgetError::Other(format!("image {}: {err}", path.display())))
}

/// AccountsService's icon, then `~user/.face`. Nothing readable is no
/// error: the greeter user cannot look into a 0700 home.
fn avatar_paths(user: &str, passwd: &str) -> Vec<PathBuf> {
    if user.is_empty() || user.contains('/') || user.starts_with('.') {
        return Vec::new();
    }
    let mut paths = vec![Path::new("/var/lib/AccountsService/icons").join(user)];
    if let Some(home) = home_in(passwd, user) {
        paths.push(home.join(".face"));
    }
    paths
}

fn home_in(passwd: &str, user: &str) -> Option<PathBuf> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != user {
            return None;
        }
        fields.nth(4).filter(|home| !home.is_empty()).map(PathBuf::from)
    })
}

fn show_avatar(image: &gtk::Image, user: &str, size: Option<i32>) {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let found = avatar_paths(user, &passwd)
        .into_iter()
        .find_map(|path| gdk::Texture::from_filename(path).ok());
    match found {
        Some(texture) => image.set_paintable(Some(&texture)),
        None => image.clear(),
    }
    if let Some(size) = size {
        image.set_pixel_size(size);
    }
}

impl WidgetDef for ImageDef {
    fn kind(&self) -> &'static str {
        "image"
    }

    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError> {
        let size = node.props.int("size")?.map(|n| n as i32);
        let sources = (node.props.str("file")?, node.props.str("icon")?, node.props.str("source")?);
        let image = match sources {
            (Some(file), None, None) => {
                let path = ctx.resolve_path(Path::new(&file));
                gtk::Image::from_paintable(Some(&paintable(&path, size.unwrap_or(ICON_SIZE))?))
            }
            (None, Some(icon), None) => load_icon(&icon)?,
            (None, None, Some(source)) if source == "avatar" => {
                let image = gtk::Image::new();
                show_avatar(&image, &ctx.app.username(), size);
                let weak = image.downgrade();
                ctx.bus.subscribe(move |event| {
                    if let UiEvent::UsernameChanged(user) = event {
                        if let Some(image) = weak.upgrade() {
                            show_avatar(&image, user, size);
                        }
                    }
                });
                image
            }
            (None, None, Some(source)) => {
                return Err(WidgetError::Other(format!(
                    "`source`: unknown source `{source}` (avatar)"
                )))
            }
            _ => {
                return Err(WidgetError::Other(
                    "exactly one of `file`, `icon` or `source` is required".into(),
                ))
            }
        };
        if let Some(size) = size {
            image.set_pixel_size(size);
        }
        Ok(image.upcast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0::/root:/bin/bash\n\
                          edraven:x:1000:1000::/home/edraven:/usr/bin/zsh\n\
                          nobody:x:65534:65534:Kernel Overflow User::/usr/bin/nologin\n";

    #[test]
    fn avatar_is_looked_for_in_accountsservice_then_the_home() {
        let paths = avatar_paths("edraven", PASSWD);
        let expected = ["/var/lib/AccountsService/icons/edraven", "/home/edraven/.face"];
        assert_eq!(paths, expected.map(PathBuf::from));
        assert_eq!(
            avatar_paths("nobody", PASSWD),
            [PathBuf::from("/var/lib/AccountsService/icons/nobody")]
        );
        assert_eq!(
            avatar_paths("ghost", PASSWD),
            [PathBuf::from("/var/lib/AccountsService/icons/ghost")]
        );
    }

    #[test]
    fn a_name_that_is_a_path_looks_nowhere() {
        assert!(avatar_paths("", PASSWD).is_empty());
        assert!(avatar_paths("../root", PASSWD).is_empty());
        assert!(avatar_paths(".hidden", PASSWD).is_empty());
        assert!(avatar_paths("a/b", PASSWD).is_empty());
    }
}
