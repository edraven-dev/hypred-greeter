# hypred-greeter

A [greetd](https://sr.ht/~kennylevinsen/greetd/) greeter where the layout is
yours. The widget tree lives in a TOML file loaded at runtime, styling is
plain GTK4 CSS, and **every widget has a stable CSS name and class** — there
is no button you cannot move, restyle, or delete, and none of it needs a
recompile.

- **Rust + GTK4, small on purpose**: no async runtime, no UI framework on
  top, ~150 crates, minutes to build from source with `paru`.
- **Widget/addon architecture**: each widget kind is a small `WidgetDef`
  implementation behind a registry; adding one (a profile menu, a battery
  readout) is a new file plus one registration line.
- **Unbrickable by config**: a broken layout, config, or stylesheet degrades
  to built-in defaults with an on-screen banner explaining what's wrong.
  Login always works.
- **Demo mode**: `hypred-greeter --demo` runs windowed in your session with
  fake auth — iterate on layout and CSS without ever leaving your desktop.

## Install

```sh
paru -S hypred-greeter        # or hypred-greeter-git
```

Point greetd at it in `/etc/greetd/config.toml`. Under
[cage](https://github.com/cage-kiosk/cage):

```toml
[default_session]
command = "cage -s -- hypred-greeter"
user = "greeter"
```

or inside a dedicated Hyprland config, run `hypred-greeter` from the
compositor and tear the compositor down when it exits (greetd starts the
chosen session only after the greeter command exits).

## Configuration

Everything lives in `/etc/greetd/hypred-greeter/` (override with
`--config`): `config.toml`, `layout.toml`, `style.css`. All three ship
documented defaults. Relative paths in `[paths]` resolve against the config
directory; `--layout` and `--style` override for quick iteration:

```sh
hypred-greeter --demo --style ./mytheme.css --layout ./mylayout.toml
```

### config.toml

| section | keys | default |
|---|---|---|
| `[paths]` | `layout`, `style` | `layout.toml`, `style.css` |
| `[background]` | `image`, `fit` (`cover`/`contain`/`fill`/`scale-down`) | none, `cover` |
| `[gtk]` | `dark`, `theme`, `icon-theme`, `cursor-theme`, `font` | unset (GTK defaults) |
| `[commands]` | `reboot`, `poweroff` (argv arrays) | `["systemctl", ...]` |
| `[sessions]` | `x11-prefix` (argv), `env` (KEY=value list) | `["startx", "/usr/bin/env"]`, `[]` |

### layout.toml — the widget tree

The root is one widget; containers nest children. Anything can go anywhere:

```toml
[root]
widget = "overlay"            # first child = base layer, rest float above

[[root.children]]
widget = "background"

[[root.children]]
widget = "clock"
format = "%A %e %B  %H:%M"
anchor = "top"
margin = [48, 0, 0, 0]

[[root.children]]
widget = "box"
name = "card"                 # style it as #card
orientation = "vertical"
spacing = 12
anchor = "center"

  [[root.children.children]]
  widget = "username"

  [[root.children.children]]
  widget = "password"
```

**Common properties** (every widget): `name` (CSS `#name`), `class` (string
or array, extra CSS classes), `halign`/`valign` (`start`/`center`/`end`/
`fill`), `anchor` (sugar for both: `center`, `top`, `bottom-right`, ...),
`hexpand`/`vexpand`, `margin` (int or `[top, right, bottom, left]`),
`width`/`height`, `visible`.

**Widgets:**

| widget | properties | notes |
|---|---|---|
| `box` | `orientation`, `spacing`, `homogeneous` | container |
| `overlay` | — | container; children after the first float, placed by `anchor` |
| `grid` | `row-spacing`, `column-spacing`; children take `col`, `row`, `col-span`, `row-span` | container |
| `label` | `text`, `wrap` | static text |
| `background` | `image`, `fit` | wallpaper; defaults from `[background]` |
| `clock` | `format` (strftime) | ticks every second |
| `username` | `placeholder` | prefilled with the last user |
| `password` | `placeholder`, `peek` | Enter submits; caps-lock warning built in |
| `message` | `text` | PAM info/errors land here |
| `session` | — | dropdown over wayland-sessions + xsessions |
| `power` | `reboot-label`, `poweroff-label`, `spacing` | runs `[commands]` |

### style.css — every selector you need

Loaded at GTK's USER priority, so it wins over the theme. Each widget gets
class `.hg-<kind>` and (unless you set `name`) the name `#hg-<kind>`:

| selector | matches |
|---|---|
| `window.hg-window` | the greeter window |
| `.hg-banner` | the config-problem banner |
| `.hg-error` | inline ⚠ placeholder for a widget that failed to build |
| `.hg-box`, `.hg-overlay`, `.hg-grid`, `.hg-label` | containers / labels |
| `.hg-background` | the wallpaper picture |
| `.hg-clock` | the clock label |
| `entry.hg-username` | username entry |
| `entry.hg-password` | password entry (GtkPasswordEntry) |
| `.hg-message`, `.hg-message.hg-message-error` | PAM messages / auth errors |
| `dropdown.hg-session`, `dropdown.hg-session > button` | session picker |
| `.hg-power button`, `#hg-power-reboot`, `#hg-power-poweroff` | power buttons |
| `#card` (or any `name` you set) | your named widgets |

GTK4 CSS supports `@define-color`, gradients, `alpha()`, borders, shadows,
animations — see the [GTK CSS docs](https://docs.gtk.org/gtk4/css-properties.html).
Parse errors are logged with file:line:col and skipped, never fatal.

## Writing a widget (addons)

Implement `WidgetDef` (`src/widgets/`), register it in
`Registry::builtin()` — done; it's addressable from layout.toml and CSS like
everything else:

```rust
pub trait WidgetDef {
    fn kind(&self) -> &'static str;                // widget = "mywidget"
    fn build(&self, ctx: &BuildCtx, node: &Node) -> Result<gtk::Widget, WidgetError>;
    fn is_container(&self) -> bool { false }
}
```

`BuildCtx` hands you config, the session list, auth actions
(`ctx.app.submit_response(...)`) and the event bus
(`ctx.bus.subscribe(...)` for PAM prompts/info/errors). The trait is
object-safe with concrete inputs so dynamically loaded plugins remain
possible later without breaking existing widgets.

## Behavior worth knowing

- **Auth flow is generic PAM, not password-only**: secret/visible prompts,
  info and error messages all flow through the same bus; MFA prompts render
  via the message widget. Try it: `--demo`, username `mfa` (and password
  `fail` for the error path).
- **State**: `/var/lib/hypred-greeter/state.toml` remembers the last user
  and each user's last session (tmpfiles.d entry ships with the package).
  Typing a known username snaps the session picker to their remembered
  session.
- **Exit codes** (the process always exits — a wedged greeter is a dark
  screen): `0` session handed to greetd, `1` startup failure, `2` greetd
  transport failure, `101` panic.

## Development

One-time setup after cloning:

```sh
git config core.hooksPath .githooks
```

That enables the tracked hooks: `pre-commit` (cargo fmt check), `commit-msg`
(conventional commits: `type(scope): subject`), `pre-push` (clippy
`-D warnings` + tests). CI runs the same three checks on every PR, plus a
weekly RustSec dependency audit.

Work lands via feature branches (`feat/...`, `fix/...`, `chore/...`) and
PRs to `main`; commits follow [Conventional Commits](https://www.conventionalcommits.org/).

## License

GPL-3.0-or-later.
