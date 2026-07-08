---
name: verify
description: Build and drive srev (the TUI itself) to observe a change end-to-end, using an isolated tmux session.
---

# Verifying srev end-to-end

srev is a full-screen ratatui/crossterm TUI — it needs a real terminal, so drive it
through tmux rather than piping stdin/stdout.

## Build

```bash
cargo build --quiet
# binary at target/debug/srev
```

## Launch in an isolated tmux session

Use a dedicated tmux socket (`-L`) so this never collides with the user's own tmux:

```bash
SESSION="srev_verify_$$"
tmux -L verify_srev kill-session -t "$SESSION" 2>/dev/null
tmux -L verify_srev new-session -d -s "$SESSION" -x 200 -y 50 "./target/debug/srev ."
sleep 1
tmux -L verify_srev capture-pane -t "$SESSION" -p   # plain text snapshot
```

Send keys with `send-keys -t "$SESSION" "<key>"` (repeat the flag for a sequence,
e.g. `"l" "l" "l"`). Common ones: `C-p` (fuzzy finder), `Escape`, `Enter`, `?` (help
overlay), `d` (toggle diff/code), `c` (commit view), `Tab` (cycle focus).

Clean up when done:

```bash
tmux -L verify_srev kill-session -t "$SESSION" 2>/dev/null
tmux -L verify_srev kill-server 2>/dev/null
```

## Reading pane state

- `capture-pane -p` gives plain text — good for content/titles/layout.
- Colors (e.g. focused-pane cyan border vs unfocused dark-gray) don't show in `-p`.
  Use `capture-pane -p -e` (keeps ANSI escapes) and grep for `\x1b[38;5;NNNm` codes
  near the text you care about. Border colors used by `pane_block()` in `src/ui.rs`:
  cyan (`38;5;6`) = focused, dark gray (`38;5;8`) = unfocused.
- Real terminal cursor position (not a rendered glyph) is available via
  `tmux display-message -p -t "$SESSION" '#{cursor_x} #{cursor_y}'` — useful for
  verifying cursor-movement actions (e.g. vim-style `0`/`$`/`h`/`l` in the code pane)
  since the cursor isn't drawn as a character in the pane buffer.
- `capture-pane -p -e` output is one long line per terminal row (box-drawing
  borders included) — when isolating a specific pane's title color, find the
  substring index of the title text and scan backwards for the last
  `\x1b[38;5;NNNm` before it (a plain `grep`/`cut` on raw escape bytes is fragile
  and will throw "illegal byte sequence").

## Layout reference (src/ui.rs `draw()`)

Three panes, each focus-highlighted independently: top-left = Files/Changed/Log
pane (`Focus::Tree`), bottom-left = Symbols/Files pane (`Focus::Outline`),
right = Code/Diff/Commit pane (`Focus::Content`). Titles are prefixed `1 `/`2 `/`3 `
(see `numbered()` in `src/ui.rs`) since number-key pane jump was added.

## Gotchas

- Running the binary against the repo itself is safe/read-only for source files —
  but check `git status --short` after a verification run to make sure nothing
  unexpected got written (e.g. don't trust that blindly forever if new
  write-to-disk features are added, like config or review-state persistence).
- `cargo test` runs fine standalone but two git tests print harmless
  `fatal: 'origin' does not appear to be a git repository` noise to stderr when
  run outside a repo with an `origin` remote configured for push — ignore it,
  those tests still pass.
