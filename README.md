# ashiato

**ashiato**（足跡, "footprints") — see what your agent just touched.

A TUI file picker that shows your whole project as a flat list sorted by modification time. Files your coding agent just edited float to the top, grouped by day (Today / Yesterday, then a date header per older day), with a syntax-highlighted preview. Select files and print their paths to stdout — or hand them straight to a reviewer like [akapen](https://github.com/worldnine/akapen).

日本語版 README は [README.ja.md](README.ja.md) にあります。

ashiato is a **time browser**, not a search tool: the default view is "what changed recently". Fuzzy search belongs to fzf, and ashiato interoperates with it (see below).

## Install

```sh
cargo install ashiato
```

Or from source:

```sh
cargo build --release
# binary: target/release/ashiato
```

## Usage

```
ashiato [directory] [flags]
```

By default, `Enter` prints the selected paths to stdout and exits (the fzf model). When stdout is a pipe, the TUI draws on `/dev/tty`, so `ashiato . | xargs akapen` works without escape-sequence contamination.

| Flag | Meaning |
|---|---|
| `--sort mtime\|ctime` | initial sort key (`t` cycles mtime↓ → mtime↑ → ctime↓ → ctime↑) |
| `--show-hidden` | show dot-directories at startup (`Ctrl+h` toggles) |
| `--show-dirs` | show directories at startup (`d` toggles) |
| `--open-cmd <cmd>` | command `Enter` launches with the selection (`{}` / `{1}`… placeholders). Blocks until it exits, then rescans — the review-loop mode |
| `--alt-open-cmd <cmd>` | command `o` launches with the selection (same `{}` / `{1}`… placeholders; blocks then rescans). Without it, `o` is a dead key |
| `--filter <text>` | initial filter (same matching as `/`; e.g. `--filter .md`) |
| `--preview <on\|off\|auto>` | preview pane; `auto` (default) hides it under 80 columns |
| `--theme <name>` | syntect theme (two-face name or `.tmTheme` path) used on both light and dark backgrounds; beats `--theme-dark` / `--theme-light` |
| `--theme-dark <name>` | syntect theme on a dark background (default `Catppuccin Mocha`) |
| `--theme-light <name>` | syntect theme on a light background (default `Solarized (light)`) |
| `--light` / `--dark` | force the palette, and with it which of the two themes applies (default: auto-detect via OSC 11 at startup, then follow the terminal's light/dark switches while open; a forced palette ignores them) |
| `--files` | no TUI — print the time-sorted list to stdout (data source for fzf etc.) |
| `--format <path\|tsv>` | output format for `--files` (`tsv` = `datetime<TAB>name<TAB>path`) |
| `--since <today\|yesterday\|Nd\|Nw>` | time cutoff for `--files` and the TUI's initial view |
| `--output` | explicit alias of the default print-to-stdout behavior (mutually exclusive with `--open-cmd`) |

`.gitignore` is respected (even outside git repos), plus a small always-ignore list (`.git/`, `node_modules/`, `target/`, `__pycache__/`, `.DS_Store`). Agent directories like `.claude/` are **not** excluded — agent-generated files are exactly what you want to review.

### Config file

`$XDG_CONFIG_HOME/ashiato/config.toml` (else `~/.config/ashiato/config.toml`). Every key is optional; without the file ashiato behaves exactly as before.

```toml
[theme]
dark  = "Catppuccin Mocha"     # two-face theme name or path to a .tmTheme file
light = "Catppuccin Latte"
```

| Key | Same as | Precedence (first wins) |
|---|---|---|
| `[theme] dark` | `--theme-dark` | `--theme` > `--theme-dark` > file > `Catppuccin Mocha` |
| `[theme] light` | `--theme-light` | `--theme` > `--theme-light` > file > `Solarized (light)` |

- Which side applies follows the light/dark decision (`--light` / `--dark`, else OSC 11 at startup and the terminal's switches while open). A name that doesn't resolve falls back to that side's default.
- Values starting with `~/` are expanded (for `.tmTheme` paths). Empty or blank values count as not written.
- Unknown keys, wrong types and broken TOML stop ashiato at startup, naming the file — a typo is never silently ignored. The flip side: a config that uses a key this build doesn't know (written for a newer ashiato) stops an older one. `--help` and `--version` never read the file.

## Keys

| Key | Action |
|---|---|
| `j` / `k`, `g` / `G`, `PgUp` / `PgDn`, `Ctrl+u` / `Ctrl+d` | move |
| `Space` | select / deselect a file |
| `Enter` | print selection to stdout and exit — or launch `--open-cmd`, block, rescan |
| `o` | launch `--alt-open-cmd`, block, rescan (dead key without it) |
| `y` | copy full path(s) to the clipboard |
| `/` | incremental filter (substring match on the relative path) |
| `\` | toggle the filter off/on without erasing it (`:nohlsearch` model) |
| `t` | cycle sort order |
| `Ctrl+h` | toggle hidden (dot-directories) |
| `d` | toggle directories |
| `u` | toggle: only files with uncommitted changes (git repos; dirty rows show `+N -M`) |
| `q` | quit |

The list rescans every 2 seconds; when an agent edits files in another pane, they float up live. Cursor and selection follow by path.

Times are relative within the hour (`now`, `5m ago`, in italics) and plain `HH:MM` beyond that — "ago" appearing at all means "touched within the hour". The list respects your terminal palette: no hard-coded colors, just the default foreground with the DIM attribute for secondary text.

### Away-diff (terminal focus)

ashiato tracks terminal focus via xterm focus reporting (DECSET 1004; kitty, WezTerm, Ghostty, alacritty, xterm, Terminal.app, iTerm2, Windows Terminal; tmux needs `focus-events on`). Terminals that never emit focus events simply keep the feature dormant.

- **While the terminal is unfocused** and files change, the rows keep their normal colors; only the **untouched files' times sink further** (dark gray + dim), so the touched files' times stand out by contrast — a quiet glance at the time column shows what the agent is touching. The footer shows `[away: N changed]`. Changes stack up (deduped, first-seen order) until you come back.
- **When focus returns**, the display reverts to the normal listing instantly — no flash (the mtime sort already floated the changed files to the top).

Changes are detected by `(mtime, size, is_dir)` — an edit that keeps all three (e.g. `touch -r` restoring the mtime after a same-size write) is not visible as a change; the preview cache has the same signature, so such edits can also leave a stale preview until the next real change.

### Git integration

Inside a git work tree, ashiato marks files with uncommitted changes (spec: [git-integration-spec.md](https://github.com/worldnine/akapen/blob/main/docs/git-integration-spec.md) §4):

- A dirty row's right side shows `+N -M` fused with the freshness time (`+7 now`) — the diff's line scale + the "uncommitted" signal (spec 4-1), without crushing ashiato's time display. Committed rows keep the plain time. When the width is tight the time is sacrificed first (recency still lives in the mtime sort and the cluster headers).
- Untracked files (`??`) count as uncommitted; their `+N` is the file's own line count. Binary files show a bare `-`.
- `u` toggles the listing to uncommitted files only (footer `[uncommitted]`; the text filter `/` and `--filter` are untouched).
- Outside a repo — or with git missing — all of this is off: rows and keys behave exactly as before.

`git status` / `git diff --numstat` are queried only when the filesystem listing actually changed (signature-gated), so the every-2s refresh of a stable tree never spawns git; a snapshot can go stale until the next real change (the spec's P4 snapshot principle).

## The review loop

With `--open-cmd`, `Enter` launches your reviewer with the selected files, blocks until it exits, and rescans — freshly edited files are back on top for the next round:

```sh
ashiato . --open-cmd "akapen {} --send-agent"
```

Pair it with `--alt-open-cmd` and the `o` key runs a second command in the same flow — keep the direct review loop (Enter = akapen) and still jump into a file manager:

```sh
ashiato . --open-cmd "akapen {} --send-agent" --alt-open-cmd "yazi {}"
# Enter = akapen review → rescan, o = open in yazi (rescan on quit)
```

## fzf interop (`--files`)

ashiato's collection logic (directory resolution, ignores, sorting, time cutoff) doubles as a data source:

```sh
ashiato --files | fzf                                   # time-ordered × fuzzy search
ashiato --files --format tsv | fzf --delimiter $'\t' \
  --with-nth 1..2 --preview 'bat --color=always {3}'    # time + name, full-path preview
ashiato --files --since today | fzf                     # only today's files
ashiato --files --since 1w --filter .md | xargs akapen  # review this week's markdown
```

## herdr integration

Directory resolution order: explicit argument → herdr (`HERDR_ENV=1`: worktree root, else the workspace agent's cwd) → current directory. ashiato itself does not depend on herdr; wiring it to an agent is the wrapper's job.

The repo is also a [herdr plugin](herdr-plugin.toml): `herdr plugin link <this repo>` registers the `ashiato.open` action (side-split picker; bind it to a key via `[[keys.command]] type = "plugin_action"`). Inside the picker, Enter opens akapen and `o` opens yazi (swap via `ASHIATO_OPEN_CMD` / `ASHIATO_ALT_OPEN_CMD` in `scripts/ashiato-pane.sh`). Convention: herdr integration code lives in the tool's repo as a plugin — see also akapen's `akp` plugin.

## Design notes

- Rust + [ratatui](https://ratatui.rs) 0.30; file collection via the `ignore` crate.
- Preview reads at most the first 256 KB; binaries show `file(1)`-style info instead of crashing.
- Speed-first preview: while you hold j/k (key repeat) the pane shows a placeholder so each cursor move costs only the list draw; the real preview catches up ~40 ms after you release the key.
- Light/dark auto-detection queries the terminal background with OSC 11 (falls back to dark). While open, ashiato subscribes to the terminal's color-scheme notifications (mode 2031) and swaps the syntax theme, the UI colors and the preview when the terminal switches (e.g. the macOS appearance flips). Terminals without mode 2031 just keep the startup decision. The subscription is dropped before a child command (akapen, …) gets the terminal and on exit, and renewed on return.
- The light/dark plumbing — OSC 11, mode 2031, the input reader that can receive its notifications (crossterm 0.29 can't), theme resolution and the `[theme]` table — comes from [termtheme](https://github.com/worldnine/termtheme).
- Deliberately not: tree views, preview scrolling, fuzzy matching, alphabetical sort, icon fonts. See [docs/spec.md](docs/spec.md).

## License & credits

MIT License.

`src/highlight.rs` is a copy of [akapen](https://github.com/worldnine/akapen)'s highlighter (its theme part now lives in termtheme), which in turn adapts [herdr-reviewr](https://github.com/persiyanov/herdr-reviewr) (MIT, Dmitry Persiyanov).

## Git integration demo

This line was added to demonstrate the `+N -M` uncommitted marker.

- edit: tracked file → `+1`
- the untracked file `docs/git-demo.md` → `+N` (its own line count)
