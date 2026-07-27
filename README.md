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

### Custom search results

A prefix can also produce its own list of rows. Give it `get_results` instead of `command`, and it runs that command with the query, reads a JSON payload from its stdout, and shows one row per entry. `action` runs on whichever row you pick.

```toml
[[spotlight.prefixes]]
prefix = "search"
label = "Web search"
get_results = "search_command {query}"
action = "xdg-open {value}"
delay = 0.5      # seconds of quiet typing before the command runs
icon_size = 22   # bigger when the rows carry artwork rather than glyphs
```

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

Custom actions can also be added, edited, deleted, reordered, and configured with Run on each from Settings -> Actions. Changes are saved back to `config.toml` and take effect immediately for context menus. The editor shows command variables that can be used in custom commands: `{path}`, `{name}`, `{parent}`, `{stem}`, `{extension}`, `{uri}`, and `{kind}`.

Custom actions appear in file, folder, and empty-folder-space context menus when every selected target matches at least one configured filter. Empty `filters` match everything. By default, IoExplorer runs the configured command once with all selected or current paths expanded as shell-quoted arguments, using the current folder as the working directory. If a command does not use any variables, the selected paths are appended as final arguments. If variables are used, placeholders such as `{path}` expand to all selected entries. Set `run_on_each = true` to run the command once per entry instead. Supported filters include glob patterns such as `*.txt`, the folder keyword `folder/`, and common type groups such as `image/*`, `video/*`, `audio/*`, and `text/*`.

## Roadmap

- Richer file operations.
- Split panes and saved layout profiles.
- Filtering.
- Network/provider plugins.
