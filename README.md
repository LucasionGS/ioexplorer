# IoExplorer

IoExplorer is a Wayland-native GUI file manager aimed at Hyprland-oriented Linux distributions (But should work on any Wayland compositor). The implementation uses Rust and GTK4 so file drag-and-drop, monitor integration, theming, and desktop launching go through GTK/GDK's native Wayland backend instead of custom protocol glue.

It is developed with customization and ricing in mind, as well as efficiency navigating of your file system.

## MVP Scope

- Local filesystem provider with a provider registry shaped for future SMB, SFTP, cloud, or virtual providers.
- Left sidebar for common local places.
- Main content area with List View and Icon View.
- Top bar with back, forward, up, refresh, breadcrumbs, and an editable path/URL entry.
- Native GTK/GDK drag-and-drop for local files using standard file-list/URI data.
- XDG config loading and optional user CSS for distribution theme customization.
- Desktop integration metadata and packaging scaffolds.
- Graphical settings page with General, View, and an Actions editor.
- Custom configurable context-menu actions for files and folders.

## Dependencies

Install a Rust toolchain plus GTK4 development libraries. On Arch-derived systems:

```sh
sudo pacman -S rust gtk4 glib2 pkgconf desktop-file-utils appstream flatpak flatpak-builder
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

## Desktop Portal File Chooser

IoExplorer includes an `ioexplorer-portal` backend for `org.freedesktop.impl.portal.FileChooser` so portal-aware apps can use IoExplorer for Open and Save dialogs.

Install the two binaries plus the portal metadata in the standard locations:

```sh
cargo build --release
install -Dm755 target/release/ioexplorer ~/.local/bin/ioexplorer
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

In icon view, use Ctrl+scroll to resize file entries. The chosen icon size is saved in `~/.local/state/ioexplorer/state` and overrides the configured `icon_size` on later launches.

Custom actions can also be added, edited, deleted, reordered, and configured with Run on each from Settings -> Actions. Changes are saved back to `config.toml` and take effect immediately for context menus. The editor shows command variables that can be used in custom commands: `{path}`, `{name}`, `{parent}`, `{stem}`, `{extension}`, `{uri}`, and `{kind}`.

Custom actions appear in file, folder, and empty-folder-space context menus when every selected target matches at least one configured filter. Empty `filters` match everything. By default, IoExplorer runs the configured command once with all selected or current paths expanded as shell-quoted arguments, using the current folder as the working directory. If a command does not use any variables, the selected paths are appended as final arguments. If variables are used, placeholders such as `{path}` expand to all selected entries. Set `run_on_each = true` to run the command once per entry instead. Supported filters include glob patterns such as `*.txt`, the folder keyword `folder/`, and common type groups such as `image/*`, `video/*`, `audio/*`, and `text/*`.

## Roadmap

- Richer file operations.
- Tabs, split panes, and saved layout profiles.
- Filtering.
- Network/provider plugins.
- Theme editor.
