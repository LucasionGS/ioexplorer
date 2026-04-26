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
```

The bundled CSS lives in `data/styles/ioexplorer.css`. Distribution maintainers can override or layer styling through `custom_css`.

## Roadmap

- Richer file operations.
- Tabs, split panes, and saved layout profiles.
- Filtering.
- Network/provider plugins.
- Graphical settings and theme editor.
