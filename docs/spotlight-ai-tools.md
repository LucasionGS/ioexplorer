# Spotlight AI — tool use

An AI provider can be given tools, so it can act rather than only answer. Tools
are **off by default**: a provider that can touch your machine is a different
thing from one that can only reply, and enabling the chat does not enable them.

```toml
[[spotlight.ai]]
prefix = "claude"
provider = "claude"
api_key_file = "~/.config/ioexplorer/anthropic-key"
builtin_tools = true      # the built-in set, minus run_command
run_command = false       # gated separately; see below
web_search = true         # Anthropic's server-side search and fetch
```

| Key | Default | Meaning |
| --- | --- | --- |
| `builtin_tools` | `false` | Enable the built-in tools |
| `run_command` | `false` | Additionally allow arbitrary shell commands |
| `web_search` | `false` | Declare Anthropic's server-side web search and fetch |
| `[[spotlight.ai.tools]]` | none | Your own tools — see [Custom tools](#custom-tools) |

## The approval model

Every tool is either **read-only** or **side-effecting**, and that decides
whether you are asked.

| Effect | Behaviour |
| --- | --- |
| Read-only | Runs automatically, on a background thread |
| Side-effecting | Shows an approval card and waits |

The card shows the **expanded** command — the exact line the shell would see —
not the template it came from. A card showing `playerctl-search {query}` would
hide the one part an injected argument controls.

`Enter` runs it, `Escape` declines. A declined call is reported back to the
model as a result saying you declined, so it can adapt instead of waiting for an
answer that never arrives. `Ctrl+C` abandons the whole round.

While a card is up, only Enter and Escape are taken — everything else still
scrolls the transcript and types in the entry, so you can read what is about to
run before deciding.

## Built-in tools

| Tool | Effect | What it does |
| --- | --- | --- |
| `search_files` | read-only | Finds files under your home directory by name |
| `list_directory` | read-only | Lists a directory |
| `read_file` | read-only | Reads a text file, subject to the limits below |
| `calculate` | read-only | Evaluates an expression |
| `list_apps` | read-only | Lists installed applications |
| `open_path` | side-effecting | Opens a path in the file manager |
| `launch_app` | side-effecting | Launches an application |
| `run_command` | side-effecting | Runs a shell command — **requires `run_command = true`** |

`run_command` is gated separately from the rest because no other built-in can do
unbounded damage, and it is the tool a prompt-injection payload would aim for.
Turning on `builtin_tools` never turns it on.

### What `read_file` refuses

Two layers, in order:

1. **Confinement.** The path is canonicalised — resolving `..` and symlinks —
   and then must still be inside your home directory. `~/Documents/../.ssh/id_rsa`
   is inside by prefix and outside by intent; only the resolved path tells them
   apart.
2. **A credential denylist.** `.ssh`, `.gnupg`, `.aws`, `.kube`, `.docker`,
   `.password-store`, `.pki`, `keyrings`, this application's own config
   directory, any `.env*`, `.netrc`, shell history, and anything ending in
   `.pem`, `.key`, `.p12`, `.pfx`, `_rsa` or `_ed25519`.

Files over 256 KiB are refused with their size rather than truncated — a model
reasoning about half a file as though it were whole is worse than one told it
cannot have it.

## Custom tools

A command template with typed parameters. The JSON Schema the API needs is
generated from the parameters, so nobody writes JSON Schema in TOML.

```toml
[[spotlight.ai.tools]]
name        = "play_music"
description = "Play a song or artist in your music player"
command     = "playerctl-search {query}"
confirm     = "always"             # "always" (default) | "never"

  [[spotlight.ai.tools.params]]
  name        = "query"
  type        = "string"           # string | integer | number | boolean
  description = "Song, album or artist to play"
  required    = true
```

Each parameter is substituted as `{name}` and **always shell-quoted**. The
values are model output, and a model that has just read a file or a web page can
be steered by its contents, so this is the boundary that has to hold even when
the model is working against you. Substitution is a single left-to-right pass: a
value that happens to contain `{another_param}` stays data.

A missing required parameter is an error rather than an empty argument; a
missing optional one expands to an explicit empty argument. An undeclared
`{placeholder}` is left in the command exactly as written.

`confirm = "never"` downgrades that one tool to auto-run. It is a per-tool
choice, not a general bypass — the built-in side-effecting tools always ask.

## Server-side web search

`web_search = true` declares Anthropic's `web_search_20260209` and
`web_fetch_20260209`. These run on Anthropic's infrastructure: nothing executes
here, so there is no approval gate and no local risk. `web_fetch` only retrieves
URLs already present in the conversation.

Two consequences worth knowing:

- **Claude only.** Ollama has no server-side web tools. Setting `web_search` on
  an Ollama provider logs a warning and is ignored rather than silently doing
  nothing.
- These versions have dynamic filtering built in, so `code_execution` is
  deliberately *not* declared alongside them — a second execution environment
  confuses the model.

A server-side tool failure arrives as a normal HTTP 200 whose result block
carries an object instead of the usual list; it is logged and the turn carries
on, rather than being treated as a transport error.

## Limits

| Limit | Value |
| --- | --- |
| Tool rounds per turn | 10 |
| `pause_turn` continuations | 5 |
| `read_file` size | 256 KiB |
| Entries from `list_directory` / `search_files` / `list_apps` | 200 |
| `search_files` depth / entries visited | 6 / 20,000 |

Exceeding the round cap answers every outstanding call with a refusal — the API
rejects a follow-up that leaves any `tool_use` unanswered — and stops with a
note.

## How it runs

The worker thread does **one HTTP request and exits.** When the model asks for a
tool it reports the call and stops; the main thread runs it, records the result,
and starts a fresh request. That keeps the worker trivial and makes approval and
cancellation ordinary main-thread work.

Execution is split by cost:

- **Read-only tools run on a worker.** They do real I/O, and a `read_file` on a
  stalled network mount must never freeze the overlay — which uses
  `KeyboardMode::Exclusive`, so a blocked main loop cannot even be escaped.
- **Side-effecting tools run on the main thread**, after approval. Every one is
  a fire-and-forget spawn that returns immediately, and two of them need GTK,
  which is main-thread-only.

Requests set `disable_parallel_tool_use`, so the model asks for one tool at a
time — one card, one command. The loop still tracks a queue of calls and answers
all of them in a single following turn, because the API rejects a round where
any `tool_use` lacks a matching `tool_result`; that correctness does not depend
on the flag staying set.

## Prompt injection

A model that reads a file or a web page can be steered by what it finds there.
That is the threat this design is shaped around:

- `run_command` ships disabled and is gated separately.
- Side-effecting tools always confirm, and the card shows the expanded command.
- Every custom-tool argument is shell-quoted; none can end the quoting.
- `read_file` is confined and denylisted, so the obvious exfiltration targets
  are not reachable in the first place.
- Tool output is rendered as a label's plain text, never as markup.

There is deliberately no "trusted" path that skips the card.

## Known gaps

- **Ollama tool support is untested here** and depends on the model: only some
  (llama3.1+, qwen2.5, mistral-nemo) honour the `tools` parameter at all. A
  model that ignores it simply never calls anything. Ollama also issues no call
  ids, so they are synthesized per turn.
- **`pause_turn` resumption replays the assistant text only.** The server-side
  tool blocks that a resume is documented to detect are not reconstructed from
  the stream, so a resumed server-side search may restart rather than continue.
