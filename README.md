# IoExplorer

IoExplorer is a Wayland-native GUI file manager aimed at Hyprland-oriented Linux distributions (But should work on any Wayland compositor). The implementation uses Rust and GTK4 so file drag-and-drop, monitor integration, theming, and desktop launching go through GTK/GDK's native Wayland backend instead of custom protocol glue.

It is developed with customization and ricing in mind, as well as efficiency navigating of your file system.

## MVP Scope

- Local filesystem provider with a provider registry shaped for future SMB, SFTP, cloud, or virtual providers.
- Left sidebar for common local places.
- Main content area with List View and Icon View.
- File pane tabs with per-tab navigation history.
- Top bar with back, forward, up, refresh, breadcrumbs, and an editable path/URL entry.
- Native GTK/GDK drag-and-drop for local files using standard file-list/URI data.
- XDG config loading and optional user CSS for distribution theme customization.
- Desktop integration metadata and packaging scaffolds.
- Graphical settings page with General, View, Theme, and an Actions editor.
- Custom configurable context-menu actions for files and folders.
- Theme editor with live UI updates and managed local CSS generation.

## Dependencies

Install a Rust toolchain plus GTK4 development libraries. On Arch-derived systems:

```sh
sudo pacman -S rust gtk4 gtk4-layer-shell glib2 pkgconf desktop-file-utils appstream flatpak flatpak-builder
```

## Build And Run

```sh
cargo run
```

To force the Wayland backend during development:

```sh
GDK_BACKEND=wayland cargo run
```

The file selector mode can be launched directly for testing:

```sh
cargo run -- --chooser --chooser-mode open
cargo run -- --chooser --chooser-mode save --current-name example.txt
```

The start menu binary can be launched directly as a one-shot popup:

```sh
cargo run --bin ioexplorer-start
cargo run --bin ioexplorer-start -- --left --top
cargo run --bin ioexplorer-start -- --center --top
cargo run --bin ioexplorer-start -- --center
```

To keep a layer-shell start menu process running and toggle it from later invocations:

```sh
cargo run --bin ioexplorer-start -- --server
cargo run --bin ioexplorer-start
cargo run --bin ioexplorer-start -- --left --top
```

In server mode, later `ioexplorer-start` calls send the requested placement to the running instance and exit immediately.

The desktop runs as a long-lived process, one layer surface per output:

```sh
cargo run --bin ioexplorer-desktop
cargo run --bin ioexplorer-desktop -- --windowed   # plain window, for nested or X11 development
```

## Spotlight

`ioexplorer-spotlight` is a keyboard-driven launcher. It opens centred and slightly above the middle of the screen, and grows downward as results appear.

```sh
cargo run --bin ioexplorer-spotlight
```

Like the start menu it can run as a daemon, so the same command toggles it open and closed:

```sh
cargo run --bin ioexplorer-spotlight -- --server
cargo run --bin ioexplorer-spotlight   # opens
cargo run --bin ioexplorer-spotlight   # closes
```

Bind that second command to a hotkey in your compositor for a Spotlight-style launcher.

### Keyboard

| Keys | Action |
| --- | --- |
| `↑` / `↓`, `Ctrl+P` / `Ctrl+N` | Move the selection |
| `Page Up` / `Page Down` | Move by eight rows |
| `Ctrl+Home` / `Ctrl+End` | Jump to the first or last result |
| `Enter` | Activate the selected result |
| `Ctrl+Enter` / `Shift+Enter` | Secondary action (open the parent folder, run in a terminal) |
| `Tab` | Accept a prefix, or complete the selected path |
| `Alt+1` … `Alt+9` | Activate that row directly |
| `Esc` | Close |

With no prefix, spotlight searches installed applications, your XDG user folders, and your IoExplorer bookmarks, ranked by fuzzy match quality and how often you launch each entry.

### Prefixes

| Prefix | Action |
| --- | --- |
| `!` | Run a shell command (`Ctrl+Enter` runs it in a terminal) |
| `>` | Browse to a path, with `Tab` completion |
| `=` | Evaluate an expression; `Enter` copies the result |
| `/` | Search your folders and bookmarks by filename |
| `w` | Switch to an open window |
| `ssh` | Connect to a host from `~/.ssh/config` |
| `vpn` | Connect, disconnect, or pick a location (only with a supported VPN client installed) |
| `pw` | Search a password vault and copy a secret (off until enabled) |
| `install` | Install an app, by category |
| `?` | List every available prefix |

Add your own prefixes in `~/.config/ioexplorer/config.toml`. `{query}` is substituted shell-quoted, and `{query_url}` percent-encoded for use inside a URL. A template with neither placeholder gets the quoted query appended.

```toml
[spotlight]
width = 640          # card width in pixels
top_ratio = 0.22     # distance from the top of the screen, as a fraction of its height
result_limit = 12
disabled_builtins = []   # e.g. ["/"] to drop the file search prefix, ["ssh"] the SSH one

[spotlight.windows]
enabled = true       # the window switcher
prefix = "w"
in_search = true     # also offer open windows on an unprefixed query

[spotlight.vpn]
enabled = true       # the VPN prefix, when a supported client is installed
prefix = "vpn"

[spotlight.passwords]
enabled = false      # the password-manager prefix; off unless you turn it on
prefix = "pw"

[[spotlight.prefixes]]
prefix = "g"
label = "Google search"
command = "xdg-open 'https://google.com/search?q={query_url}'"

[[spotlight.prefixes]]
prefix = "top"
label = "Process monitor"
command = "htop"
terminal = true
```

Alphanumeric prefixes such as `g` require a following space, so typing `go` still searches normally; typing `g` on its own offers the prefix as a `Tab`-completable row. Symbolic prefixes bind directly, so `=1+2` works without a space. Declaring a prefix that matches a built-in overrides it.

Newly installed applications appear without restarting the daemon. Launch history is kept in `~/.local/state/ioexplorer/spotlight-usage.toml`.

Known limitation: because spotlight intercepts `Enter` before the text entry sees it, IME candidate confirmation for CJK input is not supported.

### Switching windows

`w` lists the windows that are currently open, most-recently-used first, and `Enter` switches to one — moving to whichever workspace it is on rather than launching a second copy.

Open windows also appear on ordinary searches, ranked above the entry that would launch the app, so typing `disc` reaches the Discord you already have running. Set `in_search = false` to keep them behind the `w` prefix only. They are deliberately left out of the opening state, where a dozen windows would crowd out the applications that list exists to show.

Each row names the app, its workspace, and its output, and marks XWayland clients. The preview panel beside the list shows the app's icon at full size with the window's full title underneath — useful when a browser has six windows whose titles the row has to ellipsize.

| Compositor | Support |
| --- | --- |
| Hyprland | Full, via `hyprctl`. Both the current Lua dispatcher and the pre-0.56 form are handled |
| sway | Via `swaymsg`. Written to the documented IPC schema but untested |
| Anything else | The prefix explains that it needs a supported compositor |

There is no generic Wayland path, and that is a protocol limitation rather than an omission: a Wayland client cannot see or focus another client's windows at all. Doing it without a compositor-specific interface means implementing `wlr-foreign-toplevel-management-v1` or `ext-foreign-toplevel-list-v1` as a raw Wayland client.

The preview is the app's icon rather than a live thumbnail of the window. A screenshot is not obtainable here: output capture (`grim`) would include spotlight's own full-screen overlay in the shot, and windows on inactive workspaces are not being rendered at all. Real per-window thumbnails need `hyprland-toplevel-export-v1`, which is a Wayland protocol client rather than a command-line call.

### SSH connections

`ssh` lists the hosts in `~/.ssh/config`, and `Enter` opens your terminal on the connection. Typing after the prefix fuzzy-matches the alias, the hostname, the user and the `ProxyJump`, so `ssh prod` reaches `prod-db-primary`. `Ctrl+Enter` copies the `ssh` command instead of running it, and `Tab` completes the selected alias.

The first row is always the host you typed, so a machine that is not in your config is one line away — `ssh 10.0.0.5` or `ssh deploy@build.example.com` connects directly. It steps aside when the text names a configured host, since that entry already goes there. Hosts you connect to often rise to the top of the list, through the same launch history the rest of spotlight uses.

The preview panel shows the entry as the file declares it: every keyword in the block, then the file it came from — which matters once `Include` is involved and the answer to "where is this host defined" is no longer obvious.

The config is read the way ssh reads it — keywords are case-insensitive, `=` may stand in for the space, `Include` is spliced in where it appears (globs in the final path component included), and the first value for a keyword wins. Two things are deliberately not done, because both depend on the connection being made rather than on the file: `Match` blocks are not evaluated, and the defaults in a `Host *` block are not folded into every entry. Wildcard and negated patterns are not listed either — `Host web-*` is a rule about other hosts, not a host you can connect to. Nothing here decides what ssh will do; the alias is handed to `ssh`, which resolves the configuration properly.

An ad-hoc destination is checked before it is used: no leading `-`, no whitespace, and only the characters a destination is made of. That is not tidiness — ssh reads a leading dash as an option, and `-oProxyCommand=…` would run an arbitrary command, which shell quoting does nothing about.

### VPN

`vpn` reports your VPN and drives it. The first row is whatever the current state calls for — `Connect` to the best location when it is down, `Disconnect` when it is up — with the client's own status line underneath and its full reply in the preview panel. Every location the client offers follows, fuzzy-matched on the city, the region and the server's own name, so `vpn tokyo` and `vpn us east` both reach the right rows. `Enter` runs the change in the background; `Ctrl+Enter` runs it in a terminal, which is worth it for a connect that takes a few seconds and prints as it goes. Locations you pick often rise to the top through the same launch history as everything else.

The prefix only exists when a supported client is installed — no VPN on the machine means no VPN prefix, rather than one that explains its own uselessness. Detection is a `PATH` lookup for each provider's client:

| Provider | Client | Status |
| --- | --- | --- |
| `windscribe` | `windscribe-cli` | Supported |

```toml
[spotlight.vpn]
enabled = true          # the prefix
prefix = "vpn"
provider = "windscribe" # optional; detected from what is installed when unset
```

Naming a `provider` pins the choice for a machine with more than one client installed, or one whose client detection would not have picked. A name no provider answers to is refused and logged rather than quietly falling back to detection — the fallback is the one outcome that was not asked for.

Neither reply is a machine format: the client has no JSON mode, so the status is read as `Key: value` pairs and a location line is split from its ends inwards. That parsing is written to degrade rather than break — a field the client stops printing costs that field alone, and the status text is kept verbatim for the preview, so anything not interpreted is still in front of you. Both queries run on a worker thread under a five-second deadline, because a VPN client talks to a background daemon and a daemon that stops answering leaves the client waiting rather than exiting.

### Password managers

`pw` searches your password vault and copies a secret out of it. Each match is one row — the entry's name, with its login, its folder and its URL underneath — and `Enter` copies the password, `Ctrl+Enter` the username. An entry that carries a one-time-password field gains a second row directly beneath it that copies the current code.

Unlike every other prefix here, this one is off until you ask for it. The VPN prefix appears wherever a client is installed because the worst it can do is report a disconnected tunnel; this one sends a long-lived API token to a remote vault on your behalf, and that is not a decision a default gets to make.

| Provider | Server | Talks to | Status |
| --- | --- | --- | --- |
| `passwork` | Passwork 7 and later | `passwork-cli` | Supported |
| `passwork-v4` | Passwork 6 and earlier | the v4 HTTP API directly | Supported, no one-time codes |

**Which one you need depends on your server, and getting it wrong is the most likely first failure.** Passwork replaced its API wholesale between major versions and the two share no endpoint. `passwork-cli` — every released version of it — speaks only the v1 API that Passwork 7 introduced; against an older server every request 404s to an HTML page and the client dies trying to parse it. There is no CLI for v4, so that provider talks to the vault itself. If you are unsure which you have, open `https://your-vault/api/v4/info` in a browser: a page means v4, a 404 means v7.

`provider` is detected from what is installed only for CLI-backed providers. `passwork-v4` needs nothing on `PATH`, so it is never auto-detected — name it explicitly or you will get the v1 provider.

```toml
[spotlight.passwords]
enabled = true
prefix = "pw"
provider = "passwork"   # or "passwork-v4"; detected from what is installed when unset

# Optional for `passwork`, required for `passwork-v4`.
host = "https://vault.example.com"
token_command = "secret-tool lookup passwork token"

# `passwork` only.
master_key_command = "pass passwork/master-key"
refresh_token_command = "secret-tool lookup passwork refresh-token"
```

There are two ways to supply credentials, and they compose. The plain one is the environment: export `PASSWORK_HOST` and `PASSWORK_TOKEN` in the systemd user unit or your shell profile, write nothing above, and `passwork-cli` picks them up itself. The other is the `*_command` keys — a shell command per credential, run when a search needs one. Anything you do not name is left to the inherited environment, so mixing the two works: `host` in the config, the token out of your keyring. `passwork-v4` has no client to inherit an environment, so it needs `host` and `token_command` spelled out; for it, `token_command` yields the **API key**, which is exchanged for a session token and reused until it nears expiry.

The token is deliberately not a config field. `config.toml` is a plaintext file in a directory the hot-reload watcher polls, and a long-lived vault token does not belong in one. Resolved credentials are cached for five minutes so a `pass` or `gpg` command does not prompt on every keystroke, and re-resolved as soon as the config names a different command.

No secret ever reaches a command line or a result row. The search returns metadata only — names, logins, URLs — and the password itself is fetched by a separate call at the moment you press `Enter`, going straight to the clipboard. Rows are rebuilt and cloned on every keystroke, so one holding a password would leave copies of it across the process. Every client invocation is an argv with no shell involved: a shell line would put the entry name where `ps` prints it for every user on the machine, and credentials go through the environment for the same reason. A one-time code is derived inside the client from a secret this process never sees.

Searching happens on the vault's server rather than over a local snapshot — a Passwork install can hold tens of thousands of entries — so what you type is sent as a query 250 ms after you stop typing, and the results are fuzzy-ranked locally on top of that. The listing that is already on screen keeps being re-ranked while the next query is in flight, so the list does not empty and refill under you. Entries you copy from often rise to the top through the same launch history as everything else. Nothing is cached to disk, and the whole listing is dropped when the launcher is hidden.

The v4 API refuses a search shorter than two characters, so on `passwork-v4` the opening state shows your recently-used entries instead. That provider also does not offer one-time-code rows: deriving a code means implementing HMAC-SHA1 over the shared secret, which the v1 path gets for free from the CLI, and a row that cannot deliver is worse than no row.

Some Passwork instances encrypt password values in the browser and some do not; it is a per-instance setting. `passwork-v4` handles an instance with it off and refuses the other one outright, reporting that it cannot decrypt rather than putting undecryptable bytes on your clipboard and calling them a password.

Pressing `Enter` blocks the window while the client answers, under a ten-second deadline. That is deliberate and the only place in the launcher that does it: the alternative is closing the window and putting a password on the clipboard whenever the answer turns up, at a moment you are no longer thinking about it.

### Software

`install` is a two-level menu of installable applications: pick a category, pick an app, and the command that installs it runs visibly in your terminal. It ships with GIMP and Krita under Creativity, Steam and CurseForge under Gaming, Discord under Communication, and Visual Studio Code under Development — all through `yay`, so repository and AUR packages work the same way.

The levels are just text. Activating a category rewrites the search rather than closing the window, so `install creativity ` lists what is in Creativity, `Tab` completes into a category, and one backspace over the trailing space comes back out. Typing across the whole catalog works too: `install gimp` reaches GIMP without knowing which category it lives in.

```toml
[spotlight.software]
enabled = true      # the prefix
prefix = "install"
in_search = true    # also offer software on plain, unprefixed searches
keep_open = true    # hold the terminal open once the install has finished

[[spotlight.software.categories]]
id = "creativity"   # merges into the built-in category rather than replacing it

[[spotlight.software.categories.items]]
name = "Inkscape"
command = "yay -S --needed inkscape"
description = "Vector graphics"
```

With `in_search` on, typing an app's name on an ordinary search offers to install it — below every real match, and never for something already installed, since its launcher entry is the row you actually wanted. `Ctrl+Enter` copies the install command instead of running it. `keep_open` waits for `Enter` once the command exits, because a terminal opened for one command otherwise closes on its last line and takes the result with it.

Nothing here is Arch-specific beyond the commands themselves: point them at `apt`, `dnf`, `flatpak` or a script of your own and the section works the same.

See [docs/software.md](docs/software.md) for the full field reference, the merge rules, and a worked example.

### Custom search results

A prefix can also produce its own list of rows. Give it `get_results` instead of `command`, and it runs that command with the query, reads the rows from its stdout, and shows one row per entry. `action` runs on whichever row you pick.

```toml
[[spotlight.prefixes]]
prefix = "search"
label = "Web search"
get_results = "search_command {query}"
action = "xdg-open {value}"
delay = 0.5      # seconds of quiet typing before the command runs
icon_size = 22   # bigger when the rows carry artwork rather than glyphs
```

The command prints one row per line when the text is all a row needs — the line
becomes both the title and the `{value}`:

```text
~/Notes/budget.md
~/Notes/travel.md
```

For rows that carry a separate value, an icon, or a preview, it prints JSON
instead. Output starting with `{` is read as the payload, anything else as
lines.

```json
{
  "results": [
    {
      "title": "Result 1",
      "value": "https://example.com/1",
      "icon": "https://example.com/1.png",
      "preview": { "type": "image", "content": "https://example.com/1-large.png" }
    },
    { "title": "Result 2", "value": "https://example.com/2" }
  ]
}
```

`{value}` is substituted shell-quoted, and `{value_escaped}` backslash-escaped for templates that need it unquoted. `icon` is optional, drawn at the right-hand edge of the row, and may be an icon name, an absolute path, a `file://` URI, or an `http(s)://` URL — remote ones are fetched in the background and cached, so the window never blocks on the network. Without an `action`, `Enter` copies the row's value.

A row may also carry a `preview` — `type` of `text` or `image`, plus its `content` — shown in a large panel beside the card for whichever row you are hovering or have selected. Preview images load lazily and only for the row being looked at, so they can be full-size artwork rather than thumbnails.

See [docs/custom-search-results.md](docs/custom-search-results.md) for the full field reference, placeholder rules, and the limits applied to a misbehaving command.

### AI chat

Add one `[[spotlight.ai]]` block per provider. Each gets its own prefix, so you choose per query; mark one `default = true` to also offer it on plain searches.

```toml
[[spotlight.ai]]
enabled  = true
prefix   = "ai"
provider = "claude"
model    = "claude-opus-5"
default  = true
effort   = "low"          # low | medium | high | xhigh | max
max_tokens = 8192

[[spotlight.ai]]
enabled  = true
prefix   = "ol"
provider = "ollama"
model    = "llama3.2"
endpoint = "http://localhost:11434"
```

Typing `ai how do I rotate a PDF` opens a chat that grows out of the card and streams the reply.

| Keys | Action |
| --- | --- |
| `Enter` | Send a follow-up |
| `Ctrl+C` | Stop generating, keep what arrived |
| `Ctrl+Y` | Copy the last reply |
| `Esc` | Back to the search list; `Esc` again closes |

The conversation is kept in memory by the `--server` daemon, so closing and reopening spotlight resumes it. Nothing is written to disk, and it is gone when the daemon restarts. Starting a new query from the search list begins a fresh conversation.

Replies are rendered as Markdown once they finish: headings, `**bold**`, `*italic*`, `` `code` ``, `~~strikethrough~~`, bullet and numbered lists, block quotes, horizontal rules, and fenced code blocks (which scroll sideways rather than wrapping). Text streams in plain and formats when complete — a half-arrived `**bold` would otherwise show a literal `**` until its closing pair caught up.

Two deliberate limits: `_underscores_` are left alone so `snake_case_names` survive, and links render as underlined text without being clickable.

#### Tools

A provider can be given tools, so it can act rather than only answer. Tools are **off by default** — enabling the chat does not enable them.

```toml
[[spotlight.ai]]
prefix = "claude"
provider = "claude"
builtin_tools = true      # search/read/list/calculate, open, launch
run_command = false       # arbitrary shell commands; gated separately
web_search = true         # Anthropic's server-side search and fetch

[[spotlight.ai.tools]]
name    = "play_music"
command = "playerctl-search {query}"
confirm = "always"        # "always" (default) | "never"

  [[spotlight.ai.tools.params]]
  name     = "query"
  type     = "string"
  required = true
```

Read-only tools (searching, reading, listing, calculating) run automatically on a background thread. Side-effecting ones (opening, launching, running) show an approval card first and wait: `Enter` runs it, `Esc` declines. The card shows the **expanded** command — the exact line the shell would see, not the template — because that is where an injected argument would hide.

`run_command` is gated separately from the other built-ins: it is the one that can do unbounded damage, so turning on `builtin_tools` never turns it on. `read_file` is confined to your home directory after resolving `..` and symlinks, and refuses credential locations — `.ssh`, `.aws`, `.env*`, private keys, shell history, and ioexplorer's own config directory.

Custom-tool parameters are always shell-quoted; the values are model output, and a model that has just read a file or a web page can be steered by its contents.

See [docs/spotlight-ai-tools.md](docs/spotlight-ai-tools.md) for the full tool reference, the limits, and the known gaps.

**The API key never goes in `config.toml`** — the settings UI rewrites that file in full whenever you save, so a key placed there would be persisted in plaintext. There are two supported places for it.

**A key file (recommended).** `config.toml` holds only the path, which is not a secret:

```toml
api_key_file = "~/.config/ioexplorer/anthropic-key"
```

```sh
install -m600 /dev/null ~/.config/ioexplorer/anthropic-key
printf '%s' 'sk-ant-...' > ~/.config/ioexplorer/anthropic-key
```

**An environment variable.** `config.toml` holds only the variable's name:

```toml
api_key_env = "ANTHROPIC_API_KEY"   # the default for provider = "claude"
```

The daemon inherits the *compositor's* environment, so exporting in your shell profile will not reach it — put it where the session picks it up and re-login:

```sh
# ~/.config/environment.d/ioexplorer.conf
ANTHROPIC_API_KEY=sk-ant-...
```

Either way the key is read when you send a message, not at startup, so you can create the file or fix the variable without restarting anything.

To try the chat with no key and no local model, use the built-in mock provider — it runs entirely offline:

```toml
[[spotlight.ai]]
enabled  = true
prefix   = "ai"
provider = "mock"
```

Ask it something containing `slow` to watch the stream token by token, or `error` to see how a failure renders.

## Desktop

`ioexplorer-desktop` renders `~/Desktop` as a grid of icons over your wallpaper, one
layer surface per output. It paints no background of its own — wallpapers stay the
job of `swaybg`, `hyprpaper`, `swww` or whatever else you run.

```sh
systemctl --user enable --now ioexplorer-desktop.service
```

Unlike the other binaries it is single-instance: a second invocation activates the
running one and exits. Reload it in place after a config edit with
`systemctl --user reload ioexplorer-desktop`.

### Using it

Click to select, Ctrl-click to add, Shift-click for a range, drag on empty space for a
rubber band. Double-click opens: folders in IoExplorer, `.desktop` files launch, and
anything else goes to its default application. Drag an icon to move it — it snaps to
the grid unless you turn snapping off — and drag onto a folder tile to move it in.
Right-click gives the same menu the file manager uses (Copy, Cut, Paste, Rename,
Delete, Extract Here, New Folder, your custom actions), plus Arrange Icons, Sort By,
Snap To Grid, Hide Icons and Open In IoExplorer.

**Hide Icons** clears every screen at once, whichever one you use the menu on, and the
choice survives a restart. The desktop stays live while hidden — right-click still
works, so Show Icons is always reachable — and positions are untouched, so unhiding
puts everything back exactly where it was.

Every screen is live, whether or not it holds icons: right-click works anywhere and any
screen can be dropped onto. The surface keeps a tint of one part in 255 to make that
true — a window that paints nothing gives the compositor no content to hit-test, and a
screen with no icons on it would otherwise receive no pointer events at all.

**Keyboard shortcuts are unavailable on some compositors.** The desktop sits on the
`Bottom` layer so it stays under your windows, and Hyprland only grants keyboard focus
to the upper layers — measured on 0.56.2, an identical surface receives key events on
`Top` and none on `Bottom`. Everything is therefore reachable by mouse and context
menu, and Rename and New Folder open a small focused prompt rather than editing in
place. Compositors that do honour on-demand focus get F5 to refresh as a bonus.

### Icon positions

Positions live in `~/.local/state/ioexplorer/desktop-positions.toml`, not in
`config.toml`: a drag rewrites them constantly, and the config file is watched by every
running IoExplorer process, so putting them there would fire a config reload in all of
them every time you moved an icon.

They are keyed by output connector (`DP-1`, `eDP-1`), and each records both a grid cell
and — when snapping is off — an exact pixel position. The cell is what survives a
resolution change or a new panel; the pixels record a freeform placement the cell
cannot. Moving a display to a different port gives it a new key, so its icons re-flow
once.

A file exists once, so it appears on exactly one desktop. Icons start on the output
named by `output` — or the first one, if that is unset or names a screen that is not
currently connected — and **you can drag an icon onto another screen to move it there**,
after which it stays put and is recorded against that output. Each screen keeps its own
grid, snap preference and layout.

Unplugging a monitor leaves its stored layout untouched on disk and lends its icons to
the default screen meanwhile, so plugging it back in restores exactly what you had.
Moving them around while they are on loan does not steal them.

### Settings

```toml
[desktop]
icon-size = 72          # clamped to 48..=256
snap-to-grid = true     # the default for an output with no preference of its own
grid-spacing = 12       # clamped to 0..=64
show-hidden = false
folder = "/home/user/Desktop"   # defaults to XDG_DESKTOP_DIR, then ~/Desktop
output = "DP-1"         # which screen icons start on; unset means the first one
respect-panels = true   # inset the icons by whatever your bar reserved
label-backdrop = true   # translucent pill behind each label, for busy wallpapers

[desktop.sort]
key = "name"            # name, modified, created, size, or extension
descending = false
folders_first = true
```

The Snap To Grid item in the context menu is per-output and is stored beside the
positions; `snap-to-grid` here is only the starting value for an output that has never
been told otherwise.

## Desktop Portal File Chooser

IoExplorer includes an `ioexplorer-portal` backend for `org.freedesktop.impl.portal.FileChooser` so portal-aware apps can use IoExplorer for Open and Save dialogs.

Install the two binaries plus the portal metadata in the standard locations:

```sh
cargo build --release
install -Dm755 target/release/ioexplorer ~/.local/bin/ioexplorer
install -Dm755 target/release/ioexplorer-start ~/.local/bin/ioexplorer-start
install -Dm755 target/release/ioexplorer-spotlight ~/.local/bin/ioexplorer-spotlight
install -Dm755 target/release/ioexplorer-portal ~/.local/bin/ioexplorer-portal
install -Dm644 data/ioexplorer.portal ~/.local/share/xdg-desktop-portal/portals/ioexplorer.portal
install -Dm644 data/org.freedesktop.impl.portal.desktop.ioexplorer.service ~/.local/share/dbus-1/services/org.freedesktop.impl.portal.desktop.ioexplorer.service
install -Dm644 data/ioexplorer-portals.conf ~/.config/xdg-desktop-portal/portals.conf
```

Restart `xdg-desktop-portal` after installing or changing portal preference files:

```sh
systemctl --user restart xdg-desktop-portal.service
```

On custom Wayland sessions, make sure D-Bus activation has the GUI environment:

```sh
dbus-update-activation-environment --systemd DISPLAY WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_DATA_DIRS PATH
```

## Default File Manager

IoExplorer's desktop entry handles `inode/directory`, so it can be selected as the default app for folders:

```sh
xdg-mime default io.github.ionix.IoExplorer.desktop inode/directory
xdg-mime query default inode/directory
xdg-open "$HOME"
```

Some apps use the standard `org.freedesktop.FileManager1` D-Bus service for actions like "Show in folder". IoExplorer ships an opt-in service binary and a sample activation file because that generic bus name is commonly owned by Nautilus, Nemo, Dolphin, or Thunar.

For the Arch package, copy the sample into the user service directory to prefer IoExplorer without replacing another package's system file:

```sh
mkdir -p ~/.local/share/dbus-1/services
cp /usr/share/doc/ioexplorer-git/org.freedesktop.FileManager1.service ~/.local/share/dbus-1/services/org.freedesktop.FileManager1.service
dbus-update-activation-environment --systemd DISPLAY WAYLAND_DISPLAY XDG_CURRENT_DESKTOP PATH
```

Then test folder opening and item revealing:

```sh
xdg-open "$HOME"
home_uri=$(gio info -a standard::uri "$HOME" | awk '/^uri:/ {print $2}')
item_uri=$(gio info -a standard::uri /etc/hosts | awk '/^uri:/ {print $2}')
busctl --user call org.freedesktop.FileManager1 /org/freedesktop/FileManager1 org.freedesktop.FileManager1 ShowFolders ass 1 "$home_uri" ""
busctl --user call org.freedesktop.FileManager1 /org/freedesktop/FileManager1 org.freedesktop.FileManager1 ShowItems ass 1 "$item_uri" ""
```

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
desktop-file-validate data/io.github.ionix.IoExplorer.desktop
appstreamcli validate --no-net data/io.github.ionix.IoExplorer.metainfo.xml
```

## Configuration

IoExplorer reads `~/.config/ioexplorer/config.toml` when present.

```toml
default_view = "icon"
show_hidden = false
icon_size = 72
sidebar_width = 220
custom_css = "/home/user/.config/ioexplorer/theme.css"

[list_columns]
size = true
kind = true
modified = true
created = false

[sort]
key = "name"          # name, modified, created, size, or extension
descending = false
folders_first = true

[[actions]]
label = "Open in Editor"
command = "code --reuse-window"
run_on_each = false
filters = ["*.txt", "*.md"]

[[actions]]
label = "Open Terminal Here"
command = "kitty --working-directory"
filters = ["folder/"]

[[actions]]
label = "Preview Image Metadata"
command = "exiftool {path}"
run_on_each = true
filters = ["image/*"]
```

The bundled CSS lives in `data/styles/ioexplorer.css`. Distribution maintainers can override or layer styling through `custom_css`.

Settings -> Theme writes generated CSS to the configured `custom_css` path. Theme color controls support alpha, including fully transparent colors. If `custom_css` is not configured yet, IoExplorer creates `~/.config/ioexplorer/theme.css`, saves that path to `config.toml`, and applies changes immediately to the running UI. The generated CSS is kept inside a managed block so user-authored CSS can live before or after it:

```css
/* IOEXPLORER AUTO_GEN */
/* generated theme CSS */
/* /IOEXPLORER AUTO_GEN */
```

When the managed block already exists, IoExplorer replaces only that block. When no block exists, it prepends the managed block before the existing CSS.

In icon view, use Ctrl+scroll to resize file entries. The chosen icon size is saved in `~/.local/state/ioexplorer/state` and overrides the configured `icon_size` on later launches.

Listings can be sorted by name, modified date, created date, size, or extension, in either direction, from the sort button in the toolbar, from Settings -> View, or by clicking a column header in list view. Clicking the header already sorted reverses it; an arrow marks the active column. The Kind header sorts by extension, which is the grouping a type column is generally wanted for. `[list_columns]` chooses which columns appear, and so which headers are available — `created` is off by default. `folders_first` keeps directories above files whichever key is chosen; turn it off to let folders sort in with everything else. Created dates come from the filesystem's birth time and are unavailable on filesystems that do not record one. Like the view mode and icon size, the chosen order is saved in `~/.local/state/ioexplorer/state` and overrides the configured `[sort]` on later launches.

Custom actions can also be added, edited, deleted, reordered, and configured with Run on each from Settings -> Actions. Changes are saved back to `config.toml` and take effect immediately for context menus. The editor shows command variables that can be used in custom commands: `{path}`, `{name}`, `{parent}`, `{stem}`, `{extension}`, `{uri}`, and `{kind}`.

Custom actions appear in file, folder, and empty-folder-space context menus when every selected target matches at least one configured filter. Empty `filters` match everything. By default, IoExplorer runs the configured command once with all selected or current paths expanded as shell-quoted arguments, using the current folder as the working directory. If a command does not use any variables, the selected paths are appended as final arguments. If variables are used, placeholders such as `{path}` expand to all selected entries. Set `run_on_each = true` to run the command once per entry instead. Supported filters include glob patterns such as `*.txt`, the folder keyword `folder/`, and common type groups such as `image/*`, `video/*`, `audio/*`, and `text/*`.

## Roadmap

- Richer file operations.
- Split panes and saved layout profiles.
- Filtering.
- Network/provider plugins.
