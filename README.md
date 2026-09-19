# hypred-greeter

A [greetd](https://sr.ht/~kennylevinsen/greetd/) greeter where the layout is
yours. The widget tree lives in a TOML file loaded at runtime, styling is
plain GTK4 CSS, and **every widget has a stable CSS name and class** — there
is no button you cannot move, restyle, or delete, and none of it needs a
recompile.

- **Rust + GTK4, small on purpose**: no async runtime, no UI framework on
  top, ~150 crates, minutes to build from source.
- **Widget/addon architecture**: each widget kind is a small `WidgetDef`
  implementation behind a registry; adding one (a profile menu, a battery
  readout) is a new file plus one registration line.
- **Unbrickable by config**: a broken layout, config, or stylesheet degrades
  to built-in defaults with an on-screen banner explaining what's wrong.
  Login always works.
- **Demo mode**: `hypred-greeter --demo` runs windowed in your session with
  fake auth — iterate on layout and CSS without ever leaving your desktop.

## Install

Not on the AUR yet — build with `makepkg` straight from a clone:

```sh
git clone https://github.com/edraven-dev/hypred-greeter.git
cd hypred-greeter/pkg
makepkg -si -p PKGBUILD-git   # or PKGBUILD to build the pinned v0.1.0 release
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
| `[auth]` | `eager` (open the PAM conversation before anything is typed), `rearm-window` (seconds after the last input, or `"always"`: keep re-arming the reader — see [Fingerprint](#fingerprint-pam_fprintd)) | `false`, `0` |
| `[commands]` | `reboot`, `poweroff` (argv arrays) | `["systemctl", ...]` |
| `[sessions]` | `x11-prefix` (argv), `env` (KEY=value list), `default` (session id preselected for a user with nothing remembered, e.g. `"wayland/hyprland-uwsm"`) | `["startx", "/usr/bin/env"]`, `[]`, first by name |

A key the greeter does not know (a typo like `rearm_window`) keeps its
default in force and is named in the on-screen banner.

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
`width`/`height` (pixels, or a share of the window such as `"26%"` — a card
that keeps its width whatever the message says), `visible`.

**Widgets:**

| widget | properties | notes |
|---|---|---|
| `box` | `orientation`, `spacing`, `homogeneous` | container |
| `overlay` | — | container; children after the first float, placed by `anchor` |
| `grid` | `row-spacing`, `column-spacing`; children take `col`, `row`, `col-span`, `row-span` | container |
| `label` | `text`, `wrap`, `max-width-chars` | static text; a wrapping label asks for its one-line width unless capped |
| `background` | `image`, `fit` | wallpaper; defaults from `[background]` |
| `clock` | `format` (strftime) | ticks every second |
| `username` | `placeholder` | prefilled with the last user |
| `password` | `placeholder`, `peek` | Enter submits (empty: nothing), Escape cancels; read-only while a submitted password is on its way; caps-lock warning built in |
| `message` | `text`, `max-width-chars` (30) | PAM info/errors land here, wrapped to the container's width; info is shown after a 250 ms settle |
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
| `entry.hg-password.hg-password-busy` | … while a submitted password is on its way (read-only) |
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
  via the message widget. Try it: `--demo`, username `mfa` (visible OTP
  prompt + info), username `fprint` (a fingerprint cycle: two info messages
  around a 3 s blocking wait, then the password prompt), username `touch`
  (the same cycle ending in a match), and password `fail` for the error
  path.
- **Two kinds of error**: a PAM error message mid-conversation ("Failed to
  match fingerprint") is `PamError` — shown in red for at least 1.5 s, the
  entry is left alone, the conversation goes on. Only a failed conversation
  you submitted to (`AuthError`) clears and refocuses the password entry; one
  that fails on its own (pam_nologin) is reported like a PAM error.
- **State**: `/var/lib/hypred-greeter/state-vt<N>.toml` — one file per VT
  greetd runs the greeter on (`state.toml` without a VT), so a second greetd
  instance for a second user keeps its own memory — remembers the last user
  and each user's last session (tmpfiles.d entry ships with the package). A
  session id is `<kind>/<desktop file stem>`, e.g. `wayland/hyprland-uwsm`.
  Typing a known username snaps the session picker to their remembered
  session.
- **Exit codes** (the process always exits — a wedged greeter is a dark
  screen): `0` session handed to greetd, `1` startup failure, `2` greetd
  transport failure, `101` panic.

## Fingerprint (pam_fprintd)

With `pam_fprintd.so` ahead of the password modules in `/etc/pam.d/greetd`,
`[auth] eager = true` opens the PAM conversation as soon as a username is
known — the remembered user at startup, or the username entry once typing
settles (400 ms) or on Enter — so the reader is armed the moment the
greeter appears.

**With a remembered user and session a touch is the whole login** — no
Enter, no click, no key: the greeter comes up with both preselected (the
state file is written at every login; `[sessions] default` covers a user
with no session remembered yet) and the reader armed, and a matching touch
starts that session. `rearm-window = "always"` keeps it that way for
as long as the greeter is up. Typing the password works as usual; the
message widget shows pam_fprintd's own texts ("Place your right thumb on
...", "Failed to match fingerprint", "Verification timed out"). A touch
starts the session selected at that moment, so pick another first if you
want it. The conversation always belongs to the name shown: editing the
username starts a new one, clearing it ends it, and a login that completes
for a name no longer shown starts nothing.

What to expect, and why (greetd 0.10.3, pam_fprintd 1.94.5, Linux-PAM 1.7.2):

- **greetd processes one PAM step at a time.** While pam_fprintd waits for a
  finger, nothing the greeter sends is handled — a password typed meanwhile
  is kept and delivered when that wait ends. Set pam_fprintd's `timeout=` to
  the latency you accept (5 s below).
- **Only a fresh conversation re-arms the reader.** After its timeout
  pam_fprintd gives up and the conversation parks at the password prompt;
  `rearm-window` says when a parked conversation is restarted:
  - `"always"` — after every timed-out cycle, for as long as the greeter is
    up: a touch logs in whenever you walk up. The price is one PAM worker
    per pam_fprintd `timeout=` and the journal lines of each cancelled
    prompt (below), for as long as nobody logs in.
  - `N` (seconds) — for `N` seconds after your last input (the greeter
    appearing counts as input), and after that on your next input: any key
    (Escape and Enter included), pointer movement, click or touch, at most
    once every 3 s. An idle greeter goes quiet; the first touch after that
    needs a key or the mouse first.
  - `0` — never; the reader is armed once per conversation (a new one
    starts after a failed login or a username change).

  Typing into the password entry pauses all of it: while the entry holds
  text the parked prompt is kept ready, so a password typed then goes
  through at once — for 30 s after the last key; a stray character left in
  the entry does not switch the reader off for good. A conversation that
  ends without a fingerprint cycle — a failed login, or a park at once
  because the reader was claimed elsewhere or not up yet — is reopened by
  timer, so a touch works again without any input: after 3 s, doubling up
  to 60 s, three tries; under `"always"` without a limit, and a busy reader
  is retried every 6 s at most. None of these timers run with `0`.
- **Eager mode cancels PAM prompts — carry the right stack before enabling
  it.** Every restart, username change and Escape on a half-answered
  prompt drops a conversation that may sit at the password prompt. pam_unix
  reads the password through `pam_get_authtok()`, which turns a cancelled
  prompt (or an empty answer) into `PAM_AUTHTOK_ERR`. On Arch's stock
  `system-auth` that is `default=bad`, so `pam_faillock authfail` counts
  every such drop as a failed login (defaults: 3 in 15 min lock the account
  for 10 min). Inline the `system-auth` auth section into `/etc/pam.d/greetd`
  with `authtok_err=die` on pam_unix, so a prompt that never got a password
  dies before `authfail` while a wrong password (`PAM_AUTH_ERR`) still
  reaches it (`conv_err=die` alone does not help — pam_unix never returns
  it):

  ```
  auth       required     pam_securetty.so
  auth       requisite    pam_nologin.so
  auth       sufficient   pam_fprintd.so timeout=5
  auth       required     pam_shells.so
  auth       required     pam_faillock.so preauth
  -auth      [success=2 default=ignore]                            pam_systemd_home.so
  auth       [success=1 authtok_err=die conv_err=die default=bad]  pam_unix.so try_first_pass nullok
  auth       [default=die]                                         pam_faillock.so authfail
  auth       optional     pam_permit.so
  auth       required     pam_env.so
  auth       required     pam_faillock.so authsucc
  ```

  Re-check it against `/etc/pam.d/system-auth` after PAM upgrades — the
  inlined copy no longer follows it. Each dropped conversation still logs
  pam_unix's `auth could not identify password` (priority crit) and
  libpam's `conversation failed`.
- **Only the greeter on screen arms the reader.** fprintd lends the reader
  to one conversation at a time, so with several greetd instances (a second
  VT for a second user) a greeter whose VT is not the active one
  (`XDG_VTNR` vs `/sys/class/tty/tty0/active`) opens and restarts nothing,
  lets a running cycle run out, and ignores a match that arrives meanwhile
  — starting that session would pull the screen over to a VT nobody is
  looking at. It re-arms within a second of coming back on screen.
- **Escape** forgets a password submitted into a fingerprint wait (the
  entry is read-only, not disabled, while it waits) and abandons an active
  prompt (an OTP mid-way, whose text is then cleared); on a parked prompt
  it is just input (see above). **Enter on an empty entry** submits
  nothing. After a failed login the reader re-arms on your next input or
  (unless `rearm-window = 0`) after 3 s, so the error stays readable. An answer typed for a
  second-stage prompt is never reused as a password if the username
  changed meanwhile.

Try the flow without hardware: `--demo` with `[auth] eager = true` in the
config and `USER=touch` (a login with nothing pressed) or `USER=fprint`
(a timed-out cycle; add `rearm-window = "always"` to watch it re-arm).

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
