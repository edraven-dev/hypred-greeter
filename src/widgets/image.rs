use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use std::cell::RefCell;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::layout::Node;
use crate::ui::bus::UiEvent;
use crate::ui::ctx::BuildCtx;
use crate::widgets::{WidgetDef, WidgetError};

pub struct ImageDef;

/// GTK's own icon size, what a button's icon gets.
const ICON_SIZE: i32 = 16;
/// An avatar is chosen by whoever types a username, and its owner can swap
/// the file between any two looks at the path: a FIFO would block the main
/// thread in open(2) for good, a huge file or a decompression bomb is
/// decoded in full before it shows. So it is opened once, and only what
/// that open got is checked and decoded.
const AVATAR_MAX_BYTES: u64 = 1024 * 1024;
const AVATAR_MAX_SIDE: u32 = 4096;
/// libjpeg runs every scan of a progressive JPEG over the whole image: a
/// megabyte of repeated scans is half a minute of CPU at 4096 px, where
/// encoders write a few dozen at most.
const JPEG_MAX_SCANS: usize = 100;

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

fn avatar(user: &str) -> Option<gdk::Texture> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    avatar_paths(user, &passwd)
        .iter()
        .filter_map(|path| read_avatar(path))
        .find_map(|bytes| gdk::Texture::from_bytes(&bytes).ok())
}

fn read_avatar(path: &Path) -> Option<glib::Bytes> {
    let bytes = read_capped(open_avatar(path)?, AVATAR_MAX_BYTES)?;
    let (width, height) = picture_size(&bytes)?;
    (width <= AVATAR_MAX_SIDE && height <= AVATAR_MAX_SIDE).then(|| glib::Bytes::from_owned(bytes))
}

/// The file if it is a regular one of bounded size. Without O_NONBLOCK
/// opening a FIFO waits for a writer; with it the open returns and fstat
/// refuses what it got (a symlink is followed: ~/.face often is one).
fn open_avatar(path: &Path) -> Option<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    (meta.is_file() && meta.len() <= AVATAR_MAX_BYTES).then_some(file)
}

/// `None` past `cap`, however much the file grew after its size was seen.
fn read_capped(source: impl Read, cap: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    source.take(cap + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= cap).then_some(bytes)
}

/// Width and height as a PNG's or a JPEG's header gives them, the two
/// formats GTK decodes itself; `None` for anything else.
fn picture_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if let Some(chunks) = bytes.strip_prefix(b"\x89PNG\r\n\x1a\n") {
        png_size(chunks)
    } else if let Some(segments) = bytes.strip_prefix(&[0xff, 0xd8]) {
        jpeg_size(segments)
    } else {
        None
    }
}

/// IHDR comes first, as libpng requires: its length, its name, then
/// width and height.
fn png_size(chunks: &[u8]) -> Option<(u32, u32)> {
    let be32 = |at: usize| Some(u32::from_be_bytes(chunks.get(at..at + 4)?.try_into().ok()?));
    if be32(0)? != 13 || chunks.get(4..8)? != b"IHDR" {
        return None;
    }
    Some((be32(8)?, be32(12)?))
}

/// The frame header's size, the segments walked as libjpeg walks them; a
/// scan before the frame, a second frame or a segment libjpeg would not
/// skip the same way is no picture.
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let (mut at, mut size, mut scans) = (0, None, 0);
    while let Some((marker, next)) = jpeg_marker(bytes, at) {
        at = next;
        match marker {
            0xd9 => break,
            0xd8 => return None,
            0xd0..=0xd7 | 0x01 => continue,
            _ => {}
        }
        let length = usize::from(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
        let segment = bytes.get(at + 2..at + length)?;
        at += length;
        match marker {
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                let height = u16::from_be_bytes(segment.get(1..3)?.try_into().ok()?);
                let width = u16::from_be_bytes(segment.get(3..5)?.try_into().ok()?);
                if size.replace((u32::from(width), u32::from(height))).is_some() {
                    return None;
                }
            }
            0xda => {
                size?;
                scans += 1;
                if scans > JPEG_MAX_SCANS {
                    return None;
                }
            }
            _ => {}
        }
    }
    size
}

/// The next marker and the offset after it, skipping what libjpeg's
/// next_marker skips: stray bytes, 0xff fill and the stuffed 0xff 0x00
/// of entropy-coded data.
fn jpeg_marker(bytes: &[u8], mut at: usize) -> Option<(u8, usize)> {
    loop {
        at += bytes.get(at..)?.iter().position(|&byte| byte == 0xff)?;
        while bytes.get(at) == Some(&0xff) {
            at += 1;
        }
        let marker = *bytes.get(at)?;
        at += 1;
        if marker != 0 {
            return Some((marker, at));
        }
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
                let shown = RefCell::new(ctx.app.username());
                image.set_paintable(avatar(&shown.borrow()).as_ref());
                let weak = image.downgrade();
                ctx.bus.subscribe(move |event| {
                    let UiEvent::UsernameChanged(user) = event else { return };
                    // Startup announces the user this one was built with.
                    if *user == *shown.borrow() {
                        return;
                    }
                    if let Some(image) = weak.upgrade() {
                        image.set_paintable(avatar(user).as_ref());
                        shown.replace(user.clone());
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

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60,
        0x60, 0x60, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0xf6, 0x17, 0x38, 0x55, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// PNG_1X1 claiming to be `width` x `height`.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut png = PNG_1X1.to_vec();
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        png
    }

    /// SOI, an APP0 and a frame header, `scans` scans of entropy-coded
    /// data holding a stuffed 0xff and a restart marker, EOI.
    fn jpeg(width: u16, height: u16, scans: usize) -> Vec<u8> {
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 0xff, 0xda];
        jpeg.extend([0xff, 0xff, 0xc0, 0, 11, 8]);
        jpeg.extend(height.to_be_bytes());
        jpeg.extend(width.to_be_bytes());
        jpeg.extend([1, 1, 0x11, 0]);
        let scan = [0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0x12, 0xff, 0, 0xff, 0xd0, 0x34];
        jpeg.extend(scan.repeat(scans));
        jpeg.extend([0xff, 0xd9]);
        jpeg
    }

    #[test]
    fn the_size_comes_from_a_png_or_jpeg_header() {
        assert_eq!(picture_size(PNG_1X1), Some((1, 1)));
        assert_eq!(picture_size(&png(8000, 600)), Some((8000, 600)));
        assert_eq!(picture_size(&PNG_1X1[..20]), None);

        assert_eq!(picture_size(&jpeg(640, 480, 1)), Some((640, 480)));
        assert_eq!(picture_size(&jpeg(640, 480, JPEG_MAX_SCANS)), Some((640, 480)));
        assert_eq!(picture_size(&jpeg(640, 480, JPEG_MAX_SCANS + 1)), None);
        let truncated = jpeg(640, 480, 3);
        assert_eq!(picture_size(&truncated[..truncated.len() - 8]), Some((640, 480)));
        let (one_scan, frame) = (jpeg(640, 480, 1), &jpeg(64, 48, 0)[8..22]);
        let scan_first = [&one_scan[..8], &one_scan[22..]].concat();
        assert_eq!(picture_size(&scan_first), None);
        let two_frames = [&one_scan[..8], frame, &one_scan[8..]].concat();
        assert_eq!(picture_size(&two_frames), None);
        // libjpeg reads on right after a length below 2; the scans behind
        // such a segment must not go uncounted.
        let short = [&one_scan[..22], &[0xff, 0xfe, 0, 1], &one_scan[22..]].concat();
        assert_eq!(picture_size(&short), None);

        for other in [&b"GIF89a\x01\x00\x01\x00"[..], b"RIFF\0\0\0\0WEBPVP8 ", b"<svg/>", b""] {
            assert_eq!(picture_size(other), None);
        }
    }

    #[test]
    fn only_a_bounded_regular_picture_is_readable() {
        let dir = std::env::temp_dir().join(format!("hg-avatar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        assert!(read_avatar(&dir).is_none());
        assert!(read_avatar(&dir.join("missing.png")).is_none());
        assert!(read_avatar(&file("big.png", &vec![0; AVATAR_MAX_BYTES as usize + 1])).is_none());
        assert!(read_avatar(&file("junk.png", b"not a picture")).is_none());
        assert!(read_avatar(&file("wide.png", &png(AVATAR_MAX_SIDE + 1, 16))).is_none());
        assert!(read_avatar(&file("tall.jpg", &jpeg(16, 4097, 1))).is_none());
        assert!(read_avatar(&file("max.png", &png(AVATAR_MAX_SIDE, AVATAR_MAX_SIDE))).is_some());
        let one = file("one.png", PNG_1X1);
        assert_eq!(read_avatar(&one).as_deref(), Some(PNG_1X1));
        let to_one = dir.join(".face");
        std::os::unix::fs::symlink(&one, &to_one).unwrap();
        assert_eq!(read_avatar(&to_one).as_deref(), Some(PNG_1X1));

        // Opening a FIFO without a writer the plain way would wait for one.
        let fifo = dir.join("fifo");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let to_fifo = dir.join("to-fifo.png");
        std::os::unix::fs::symlink(&fifo, &to_fifo).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let paths = [fifo, to_fifo];
        std::thread::spawn(move || tx.send(paths.map(|path| read_avatar(&path).is_none())));
        let refused = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert_eq!(refused, Ok([true, true]), "a FIFO must be refused at once");

        let grows = file("grows.png", PNG_1X1);
        let checked = open_avatar(&grows).unwrap();
        let mut append = std::fs::OpenOptions::new().append(true).open(&grows).unwrap();
        std::io::Write::write_all(&mut append, &vec![0; AVATAR_MAX_BYTES as usize]).unwrap();
        assert!(read_capped(checked, AVATAR_MAX_BYTES).is_none());
        assert!(read_capped(std::io::repeat(0), AVATAR_MAX_BYTES).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_that_is_a_path_looks_nowhere() {
        assert!(avatar_paths("", PASSWD).is_empty());
        assert!(avatar_paths("../root", PASSWD).is_empty());
        assert!(avatar_paths(".hidden", PASSWD).is_empty());
        assert!(avatar_paths("a/b", PASSWD).is_empty());
    }
}
