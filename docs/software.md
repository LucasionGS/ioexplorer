# Software

The `install` prefix is a two-level menu of installable applications: pick a
category, pick an app, and the command that installs it runs visibly in your
terminal.

```
install                  → the categories
install creativity       → the categories, filtered
install creativity ␣     → the apps in Creativity
install creativity kri   → the apps in Creativity, filtered
install gimp             → GIMP, without going through its category
```

Activating a category rewrites the search text rather than closing the window,
so the levels are just text: `Tab` completes into a category, and one backspace
over the trailing space comes back out of it.

## The built-in catalog

It ships with six apps, installed through `yay` so that repository and AUR
packages work the same way.

| Category | App | Command |
| --- | --- | --- |
| Creativity | GIMP | `yay -S --needed gimp` |
| Creativity | Krita | `yay -S --needed krita` |
| Gaming | Steam | `yay -S --needed steam` |
| Gaming | CurseForge | `yay -S --needed curseforge` |
| Communication | Discord | `yay -S --needed discord` |
| Development | Visual Studio Code | `yay -S --needed visual-studio-code-bin` |

Every one of these can be replaced from the config, and nothing here is
Arch-specific beyond the commands themselves — point them at `apt`, `dnf`,
`flatpak` or a script and the section works the same.

## Configuration

```toml
[spotlight.software]
enabled = true       # the prefix
prefix = "install"
in_search = true     # also offer software on plain, unprefixed searches
keep_open = true     # hold the terminal open once the install has finished
disabled_categories = []
```

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Whether the prefix exists at all |
| `prefix` | `"install"` | The key that opens the catalog |
| `in_search` | `true` | Offer matching apps on plain searches too |
| `keep_open` | `true` | Wait for `Enter` after the install command exits |
| `disabled_categories` | `[]` | Built-in category ids to drop, e.g. `["gaming"]` |

`keep_open` exists because a terminal opened for one command closes on that
command's last line, taking the result with it. Turn it off for a package
manager that pauses on its own.

### Categories

```toml
[[spotlight.software.categories]]
id = "creativity"
label = "Creativity"
icon = "applications-graphics-symbolic"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `id` | yes | What you type to enter the category, and the key it merges on |
| `label` | no | Shown on the row. Defaults to the built-in label, or to `id` |
| `icon` | no | Icon name for the category and its apps |

A category whose `id` matches a built-in one **merges into it** rather than
replacing it, so adding a single app does not cost you the ones already there.
An unknown `id` appends a new category. A category left with no apps is not
listed.

### Apps

```toml
[[spotlight.software.categories.items]]
name = "Inkscape"
command = "yay -S --needed inkscape"
description = "Vector graphics"
keywords = ["svg", "illustrator"]
icon = "applications-graphics-symbolic"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `name` | yes | Shown on the row, and the key apps merge on |
| `command` | yes | The command line that installs it, run verbatim in a terminal |
| `description` | no | Shown beside the name. Defaults to the command |
| `keywords` | no | Extra search terms, e.g. `["photoshop"]` on an image editor |
| `icon` | no | Icon name. Defaults to the category's |

Apps merge by name, case-insensitively: reusing a built-in name replaces that
entry, a new name appends one. An app with no `name` or no `command` is skipped
and logged rather than shown as a row that does nothing.

`command` is run as written — it is not a template, and nothing is substituted
into it, because there is no user-typed text anywhere in the line.

## Worked example

Add Inkscape to Creativity, switch GIMP to Flatpak, and drop the Gaming
category:

```toml
[spotlight.software]
disabled_categories = ["gaming"]

[[spotlight.software.categories]]
id = "creativity"

[[spotlight.software.categories.items]]
name = "Inkscape"
command = "yay -S --needed inkscape"
description = "Vector graphics"

[[spotlight.software.categories.items]]
name = "GIMP"
command = "flatpak install -y flathub org.gimp.GIMP"
```

Krita is untouched, and Creativity now lists GIMP (via Flatpak), Krita and
Inkscape.

## On plain searches

With `in_search = true`, typing an app's name on an ordinary search offers to
install it, and typing `install software` offers to open the catalog. These rows
sit below every real match — an app you already have always comes first — and an
app that is already installed is left out entirely, since its launcher entry is
the row you actually want.

Installed is decided by looking for a matching desktop entry, by name and by id.
An app that ships a desktop entry resembling neither will keep being offered.

## Keys

| Key | Action |
| --- | --- |
| `Enter` | Run the install in a terminal, or enter the category |
| `Ctrl+Enter` | Copy the install command instead of running it |
| `Tab` | Complete the selected row into the search text |
| `Backspace` | Leave the category, once past its trailing space |
