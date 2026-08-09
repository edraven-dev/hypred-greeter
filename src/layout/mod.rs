//! Layout loading: TOML text → `Node` tree, with the embedded default as the
//! guaranteed fallback. A broken layout file must never make login impossible.

pub mod build;
pub mod node;

pub use node::{Node, Props};

use node::Common;
use std::path::Path;

pub const DEFAULT_LAYOUT: &str = include_str!("../../data/layout.toml");

pub struct Loaded {
    pub root: Node,
    pub problems: Vec<String>,
}

/// Load a layout file; fall back to the embedded default on any failure.
/// `require_auth`: outside demo mode a tree with no `password` widget is
/// treated as broken — a greeter you cannot log in from is worse than an
/// unstyled one.
pub fn load(path: &Path, require_auth: bool) -> Loaded {
    let mut problems = Vec::new();

    let root = std::fs::read_to_string(path)
        .map_err(|err| problems.push(format!("layout {}: {err}", path.display())))
        .ok()
        .and_then(|text| {
            info!("layout: {}", path.display());
            parse_str(&text, &mut problems)
        })
        .filter(|root| {
            let ok = !require_auth || contains_kind(root, "password");
            if !ok {
                problems.push("layout has no `password` widget — using built-in layout".into());
            }
            ok
        });

    let root = root.unwrap_or_else(|| {
        let mut default_problems = Vec::new();
        let root = parse_str(DEFAULT_LAYOUT, &mut default_problems)
            .expect("embedded default layout must parse (covered by unit test)");
        assert!(default_problems.is_empty(), "embedded default layout: {default_problems:?}");
        root
    });

    Loaded { root, problems }
}

pub fn parse_str(text: &str, problems: &mut Vec<String>) -> Option<Node> {
    let mut table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(err) => {
            problems.push(format!("layout: {err}"));
            return None;
        }
    };

    let root = match table.remove("root") {
        Some(toml::Value::Table(root)) => root,
        Some(_) => {
            problems.push("layout: `root` must be a table".into());
            return None;
        }
        None => {
            problems.push("layout: missing `[root]` table".into());
            return None;
        }
    };
    for key in table.keys() {
        problems
            .push(format!("layout: unknown top-level key `{key}` (everything nests under [root])"));
    }

    parse_node(root, "root".into(), problems)
}

fn parse_node(mut table: toml::Table, path: String, problems: &mut Vec<String>) -> Option<Node> {
    let kind = match table.remove("widget") {
        Some(toml::Value::String(kind)) => kind,
        Some(_) => {
            problems.push(format!("{path}: `widget` must be a string"));
            return None;
        }
        None => {
            problems.push(format!("{path}: missing `widget` key"));
            return None;
        }
    };

    let name = match table.remove("name") {
        None => None,
        Some(toml::Value::String(name)) => Some(name),
        Some(_) => {
            problems.push(format!("{path}: `name` must be a string"));
            None
        }
    };

    let classes = match table.remove("class") {
        None => Vec::new(),
        Some(toml::Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
        Some(toml::Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                toml::Value::String(s) => Some(s),
                _ => {
                    problems.push(format!("{path}: `class` entries must be strings"));
                    None
                }
            })
            .collect(),
        Some(_) => {
            problems.push(format!("{path}: `class` must be a string or array of strings"));
            Vec::new()
        }
    };

    let common = parse_common(&mut table, &path, problems);

    let children = match table.remove("children") {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => items
            .into_iter()
            .enumerate()
            .filter_map(|(i, item)| match item {
                toml::Value::Table(child) => {
                    parse_node(child, format!("{path}.children[{i}]"), problems)
                }
                _ => {
                    problems.push(format!("{path}.children[{i}]: must be a table"));
                    None
                }
            })
            .collect(),
        Some(_) => {
            problems.push(format!("{path}: `children` must be an array of tables"));
            Vec::new()
        }
    };

    Some(Node { kind, name, classes, common, props: Props::new(table), children, path })
}

fn parse_common(table: &mut toml::Table, path: &str, problems: &mut Vec<String>) -> Common {
    let mut common = Common::default();
    let mut bad =
        |key: &str, expected: &str| problems.push(format!("{path}: `{key}` must be {expected}"));

    if let Some(value) = table.remove("anchor") {
        match value.as_str().and_then(node::parse_anchor) {
            Some((h, v)) => (common.halign, common.valign) = (Some(h), Some(v)),
            None => bad("anchor", "one of center/fill/top/bottom/left/right/top-left/..."),
        }
    }
    // Explicit halign/valign win over anchor's sugar.
    if let Some(value) = table.remove("halign") {
        match value.as_str().and_then(node::parse_align) {
            Some(align) => common.halign = Some(align),
            None => bad("halign", "start/center/end/fill"),
        }
    }
    if let Some(value) = table.remove("valign") {
        match value.as_str().and_then(node::parse_align) {
            Some(align) => common.valign = Some(align),
            None => bad("valign", "start/center/end/fill"),
        }
    }
    for (key, slot) in [("hexpand", &mut common.hexpand), ("vexpand", &mut common.vexpand)] {
        if let Some(value) = table.remove(key) {
            match value.as_bool() {
                Some(b) => *slot = Some(b),
                None => bad(key, "a boolean"),
            }
        }
    }
    // Out-of-range values are reported, not silently wrapped by `as` casts.
    let to_i32 = |v: &toml::Value| v.as_integer().and_then(|n| i32::try_from(n).ok());
    if let Some(value) = table.remove("margin") {
        let parsed = match &value {
            toml::Value::Integer(_) => to_i32(&value).map(|n| [n; 4]),
            toml::Value::Array(items) if items.len() == 4 => {
                match items.iter().filter_map(to_i32).collect::<Vec<_>>().as_slice() {
                    [t, r, b, l] => Some([*t, *r, *b, *l]),
                    _ => None,
                }
            }
            _ => None,
        };
        match parsed {
            Some(margin) => common.margin = Some(margin),
            None => bad("margin", "an integer or [top, right, bottom, left]"),
        }
    }
    for (key, slot) in [("width", &mut common.width), ("height", &mut common.height)] {
        if let Some(value) = table.remove(key) {
            match to_i32(&value) {
                Some(n) => *slot = Some(n),
                None => bad(key, "an integer"),
            }
        }
    }
    if let Some(value) = table.remove("visible") {
        match value.as_bool() {
            Some(b) => common.visible = Some(b),
            None => bad("visible", "a boolean"),
        }
    }
    common
}

pub fn contains_kind(node: &Node, kind: &str) -> bool {
    node.kind == kind || node.children.iter().any(|child| contains_kind(child, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_parses_clean() {
        let mut problems = Vec::new();
        let root = parse_str(DEFAULT_LAYOUT, &mut problems).expect("default layout must parse");
        assert!(problems.is_empty(), "{problems:?}");
        assert!(contains_kind(&root, "password"), "default layout must allow login");
        assert!(contains_kind(&root, "background"));
    }

    #[test]
    fn problems_carry_paths() {
        let mut problems = Vec::new();
        let text = "[root]\nwidget = \"box\"\n[[root.children]]\nformat = \"%H\"\n";
        let root = parse_str(text, &mut problems).unwrap();
        assert!(root.children.is_empty());
        assert_eq!(problems, ["root.children[0]: missing `widget` key"]);
    }

    #[test]
    fn garbage_toml_reports_not_panics() {
        let mut problems = Vec::new();
        assert!(parse_str("not = [valid", &mut problems).is_none());
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn anchor_sugar_and_margin_forms() {
        let mut problems = Vec::new();
        let text = "[root]\nwidget = \"box\"\nanchor = \"bottom-right\"\nmargin = [1, 2, 3, 4]\n";
        let root = parse_str(text, &mut problems).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(root.common.halign, Some(gtk4::Align::End));
        assert_eq!(root.common.valign, Some(gtk4::Align::End));
        assert_eq!(root.common.margin, Some([1, 2, 3, 4]));
    }
}
