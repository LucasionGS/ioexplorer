# Custom search results

A spotlight prefix can ask a command for the rows to show, instead of running
one fixed command line. Set `get_results` on the prefix; the command receives
the query, prints a JSON payload on stdout, and whichever row the user picks is
substituted into `action`.

## Configuration

```toml
[[spotlight.prefixes]]
prefix = "search"
get_results = "search_command '{query}'"
action = "xdg-open {value}"
delay = 0.5
```

| Key | Required | Meaning |
| --- | --- | --- |
| `prefix` | yes | The key that activates it, e.g. `search cats` |
| `get_results` | yes | Command printing the results payload on stdout |
| `action` | no | Command run when a row is activated |
| `delay` | no | Seconds of quiet typing before the command runs. Default `0.5`, clamped to `0`–`10` |
| `label` | no | Shown in the prefix badge and the `?` listing. Defaults to the prefix key |
| `description` | no | Shown in the `?` listing. Defaults to the command |
| `icon` | no | Icon name for the rows. Defaults to `application-x-executable-symbolic` |
| `icon_size` | no | Pixel size of each row's own icon. Default `22`, clamped to `16`–`256` |
| `pagination` | no | Let the user page through the results. Requires `{page}` in `get_results` |
| `terminal` | no | Run `action` in a terminal emulator instead of detached |

`get_results` takes precedence over `command`, so a prefix is either a fixed
command or a result provider, never both.

The `delay` exists so the command is not spawned on every keystroke. Each
keystroke supersedes the pending run, which is then dropped before the process
is ever started — only the query the user stopped typing on reaches the command.

### Placeholders

In `get_results`, as in any prefix command:

| Placeholder | Substitution |
| --- | --- |
| `{query}` | The query, shell-quoted |
| `{query_url}` | The query, percent-encoded for use inside a URL |
| `{page}` | The current page number, when `pagination` is on |

In `action`, referring to the chosen row:

| Placeholder | Substitution |
| --- | --- |
| `{value}` | The row's `value`, shell-quoted |
| `{value_escaped}` | The row's `value`, with every shell metacharacter backslash-escaped |

A template with no placeholder gets the quoted query (or value) appended, so
`action = "xdg-open"` works on its own.

Both forms are injection-safe, and both survive being wrapped in quotes the
template already has: `'{value}'` expands to `''https://example.com''`, which
the shell reads as the value and nothing more.

## Results payload

The command must print a JSON object with a `results` array on stdout. Anything
on stderr is ignored.

```json
{
  "results": [
    {
      "title": "Result 1",
      "value": "https://example.com/result1",
      "icon": "https://example.com/icon1.png"
    },
    {
      "title": "Result 2",
      "value": "https://example.com/result2",
      "icon": "file:///home/user/icon2.png"
    }
  ]
}
```

| Field | Required | Meaning |
| --- | --- | --- |
| `title` | yes | The row's heading. A row without one is dropped |
| `value` | no | Substituted into `action`; also shown as the row's subtitle |
| `icon` | no | Drawn at the right-hand edge of the row |
| `preview` | no | Shown in a large panel beside the list. See below |

The command's ordering is preserved — it knows what is most relevant to its own
query. Rows beyond `spotlight.result_limit` are not shown.

An `icon` may be an icon-theme name (`folder-symbolic`), an absolute path, a
`file://` URI, or an `http://` / `https://` URL. Remote icons are downloaded on
a background thread and cached under `~/.cache/ioexplorer/spotlight-icons/`,
keyed by a hash of the URL, so the main loop never waits on the network. An icon
that cannot be fetched is simply left out.

The default `icon_size` of 22 suits a glyph. A provider returning artwork —
album covers, thumbnails, photographs — wants far more, so set `icon_size` on
the prefix. Rows grow to fit, so a larger size means fewer rows on screen before
the list scrolls; an image is drawn to fit a square of that size, keeping its
aspect ratio, and is not sharpened by asking for more pixels than it has.

## Pagination

Set `pagination` and put a `{page}` where the command wants the page number:

```toml
[[spotlight.prefixes]]
prefix = "k"
get_results = "image-search {query} {page}"
pagination = true
```

| Key | Action |
| --- | --- |
| `Alt+→` or `Ctrl+Page Down` | Next page |
| `Alt+←` or `Ctrl+Page Up` | Previous page |

Pages start at 1 and never go below it. The command is re-run for each page and
its rows replace the previous ones, so it returns one page at a time — there is
no accumulating list to scroll. The page shows in the prefix badge once you are
past the first.

Editing the query, or switching to another prefix, goes back to page 1. Paging
skips the `delay`, because a keypress is a deliberate request rather than
mid-typing noise.

Nothing tells the launcher how many pages exist — only the command knows. A page
that comes back empty is reported as the end of the list rather than as a search
that found nothing.

`pagination` without a `{page}` in `get_results` is refused and logged: every
page would run the identical command line, so it would appear to work and
change nothing.

## Preview

A row may carry a `preview`, shown in a panel to the left of the card:

```json
{
  "title": "Sunset over the bay",
  "value": "a1b2c3.jpg",
  "preview": { "type": "image", "content": "https://example.com/large.jpg" }
}
```

| Field | Meaning |
| --- | --- |
| `type` | `text`, `image`, or `icon` |
| `content` | The text to display, or the artwork to draw |
| `caption` | Optional text under the artwork. Ignored by `text` |

For `image`, `content` may be an absolute path, a `file://` URI, or an
`http://` / `https://` URL. For `icon` it is an icon-theme name such as
`firefox` — resolved through the icon theme, so there is nothing to download and
it appears immediately. For `text` it is displayed as-is — no markup, no
wrapping of long words beyond what fits.

A `caption` is for the detail that does not fit in a row: the untruncated title,
the dimensions, the author. It is drawn dimmed under the artwork.

```json
{
  "title": "Firefox",
  "value": "firefox.desktop",
  "preview": { "type": "icon", "content": "firefox", "caption": "Web browser\nWorkspace 3" }
}
```

The panel shows **one** row at a time: whichever the pointer is over, or the
selected row when the pointer is elsewhere. Nothing is shown for a row without a
preview, and the panel disappears entirely on an output too narrow to hold it
without pushing the card off centre.

Images load only when a row is actually pointed at, and only after a short pause
— holding an arrow key down does not fetch every row it travels through. A
remote image shows `Loading…` while it downloads, then replaces itself; the
bytes are cached under `~/.cache/ioexplorer/spotlight-previews/`, keyed by a
hash of the URL, so coming back to a row is instant.

Because previews are fetched lazily, they can be much larger than icons — the
full-size artwork rather than a thumbnail. The cap is 16 MiB per image.

## Activation

`Enter` runs `action`; `Ctrl+Enter` runs it in a terminal. With no `action`
configured, `Enter` copies the row's `value` to the clipboard.

## Limits

Guards against a misbehaving command, all applied per run:

| Limit | Value |
| --- | --- |
| Runtime before the command is killed | 5s |
| stdout read | 4 MiB |
| Rows kept | 200 |
| Downloaded icon size | 2 MiB |
| Downloaded preview size | 16 MiB |

If the command fails, times out, or prints something that is not the expected
JSON, a single row explains what went wrong rather than the list going blank.

## Example

A script that searches your notes:

```sh
#!/bin/sh
# ~/.local/bin/note-search
rg --files-with-matches --smart-case "$1" ~/Notes 2>/dev/null | head -20 |
  jq --raw-input --slurp '{
    results: (split("\n") | map(select(length > 0)) | map({
      title: (split("/") | last),
      value: .,
      icon: "text-x-generic-symbolic"
    }))
  }'
```

```toml
[[spotlight.prefixes]]
prefix = "n"
label = "Notes"
icon = "accessories-text-editor-symbolic"
get_results = "note-search {query}"
action = "xdg-open {value}"
```

Typing `n budget` then lists the matching notes and opens the chosen one.
