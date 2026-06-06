# srev — source reviewer

[日本語](README.md) | **English**

A terminal TUI viewer specialized for *reading* code without opening an editor.
It keeps the controls simple so you can focus on reviewing diffs and browsing full files. No editing features.

---

## Features

- **Working tree vs HEAD diff** on demand with `d` — review uncommitted changes
- **Toggle diff ⇄ full code** with `d`, preserving the corresponding line position
- **Vim-like cursor** for reading code; **jump to definition** under cursor with `gd`
- **Visual mode** (`v`/`V`) to select a range, then `y` to copy to clipboard
- **Outline pane** (bottom-left) lists symbols in the open file for quick navigation
- **In-file search** (`/` → `Enter`, `n`/`N` for next/previous match)
- **Fuzzy file search** (`Ctrl-P`, powered by nucleo — fzf equivalent)
- **Project-wide content search** (`Ctrl-F`, case-insensitive substring) — jump to a matching line
- **Inline fuzzy filtering** with `/` in tree, outline, and overlay panels
- **`Ctrl-R` reload** — re-reads the open file, git state, tree, and index while keeping cursor position
- **Syntax highlighting** for 70+ languages via inkjet / tree-sitter — **Markdown also highlights code inside fenced blocks** per language
- **Diff review navigation** — jump between hunks with `n`/`N`, between changed files with `]`/`[`. Diffs default to **side-by-side** (new/deleted files fall back to single column automatically); toggle with `s`
- File tree shows change status (`M`=modified / `A`=added / `D`=deleted / `?`=untracked) respecting `.gitignore`
- Single binary. Builds on Linux / macOS / Windows

---

## Installation / Build

Source builds only for now. You need a Rust toolchain and a **C compiler**
(required to build libgit2 and tree-sitter grammars).

```sh
git clone https://github.com/mkonishi1981/srev
cd srev
cargo build --release   # output: target/release/srev
cargo test              # run unit tests
```

The release binary is approximately **80 MB** due to bundled grammar files.
Place the binary somewhere on your `$PATH` to use it as `srev`.

---

## Usage

### Launching

```sh
srev [PATH]   # defaults to the current directory if PATH is omitted
```

- Starts in **code mode** (file tree browsing). Press `d` to switch to diff (working tree vs HEAD).

### Layout

```
┌──────────────┬──────────────────────────────┐
│  File tree   │                              │
│ (code mode)  │         Content              │
│  Changed     │   (code  or  diff)           │
│  files       │                              │
│ (diff mode)  │                              │
├──────────────┤                              │
│  Outline     │                              │
│  (Symbols)   │                              │
└──────────────┴──────────────────────────────┘
                  Status / help bar
```

- **Top-left**: file tree (code mode) or changed-file list (diff mode)
- **Bottom-left**: symbol list for the open file (Outline / Symbols)
- **Right**: content pane — code or unified diff
- **Bottom bar**: status and key hints

Press `d` to toggle between code and diff, keeping the current line in view.

### Key Bindings

#### Global

| Key | Action |
|-----|--------|
| `q` | Quit |
| `Tab` | Cycle focus (tree → outline → content) |
| `d` | Toggle diff ⇄ code (line position preserved) |
| `Ctrl-P` | Open fuzzy file search overlay |
| `Ctrl-F` | Project-wide content search (substring); `Enter` jumps to the matching line |
| `Ctrl-R` | Reload (file, git state, tree, index — cursor kept) |
| `]` / `[` | Open next / previous file (code mode = all files by path; diff mode = changed files) |

#### Tree (code mode) / Changed-file list (diff mode)

| Key | Action |
|-----|--------|
| `j` / `k`, `↓` / `↑` | Move up/down |
| `Enter` / `l` | Open file / expand directory |
| `h` | Collapse directory (tree only) |
| `/` | Inline fuzzy filter |

#### Outline (bottom-left)

| Key | Action |
|-----|--------|
| `j` / `k` | Select symbol |
| `Enter` / `l` | Jump to definition line |
| `/` | Filter symbols |

#### Content pane — code mode (vim-like)

| Key | Action |
|-----|--------|
| `h` / `j` / `k` / `l` | Move cursor |
| `w` / `b` | Word forward/back (within line) |
| `0` / `$` | Line start / end |
| `gg` / `G` | File start / end |
| `Ctrl-d` / `Ctrl-u` | Half-page scroll |
| `gd` | Jump to definition of word under cursor |
| `v` / `V` | Start visual mode (character / line) |
| `y` | Copy selection to clipboard |
| `Y` | Copy location to clipboard (no selection = `path:line:col`; single-line = `path:line`; multi-line = `path:start-end`) |
| `Esc` | Cancel selection |
| `/` → type → `Enter` | In-file search |
| `n` / `N` | Next / previous match |

> The `g` prefix for `gg` and `gd` is fixed and cannot be remapped in the config file.

#### Content pane — diff mode

| Key | Action |
|-----|--------|
| `j` / `k` | Scroll |
| `gg` / `G` | Jump to start / end |
| `PageDown` / `PageUp` | Page scroll |
| `n` / `N` | Jump to next / previous hunk (change block) |
| `s` | Toggle side-by-side ⇄ unified (default is side-by-side; new/deleted files show as single column) |

#### Overlay / filter / search input

| Key | Action |
|-----|--------|
| `Esc` | Cancel / close |
| `Enter` | Confirm |
| `Backspace` | Edit input |
| `↑` / `↓` or `Ctrl-p` / `Ctrl-n` | Move through candidates |

> `Ctrl-p` / `Ctrl-n` for candidate navigation is active **only while a filter or overlay is open**.
> In normal mode, `Ctrl-P` opens the fuzzy file search overlay.

---

## Configuration

### Remapping Keys

Config file: `~/.config/srev/config.toml`
(Override the path with the `SREV_CONFIG` environment variable.)

Add entries under `[keys]` as `"key" = "action"`.

```toml
[keys]
"ctrl-r" = "reload"
"x"      = "toggle_diff"   # bind to a different key
"d"      = "none"          # disable the default d binding
```

### Key Notation

| Type | Examples |
|------|---------|
| Single character | `"a"`, `"/"`, `"$"` |
| Ctrl modifier | `"ctrl-p"`, `"ctrl-r"` |
| Uppercase | `"Y"`, `"G"`, `"N"` |
| Named keys | `tab`, `enter`, `esc`, `space`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `backspace`, `del` |

### Action Names

| Action name | Description |
|-------------|-------------|
| `quit` | Quit |
| `focus_next` | Cycle focus |
| `down` | Move down |
| `up` | Move up |
| `left` | Move left / collapse directory |
| `right` | Move right / open |
| `activate` | Open / confirm |
| `top` | Jump to top (like `gg`) |
| `bottom` | Jump to bottom (like `G`) |
| `half_page_down` | Half-page scroll down |
| `half_page_up` | Half-page scroll up |
| `word_forward` | Word forward |
| `word_back` | Word back |
| `line_start` | Line start |
| `line_end` | Line end |
| `toggle_diff` | Toggle diff ⇄ code |
| `goto_def` | Jump to definition |
| `find` | Start in-file search |
| `search_next` | Next match |
| `search_prev` | Previous match |
| `visual_char` | Character visual mode |
| `visual_line` | Line visual mode |
| `yank` | Copy selection |
| `yank_location` | Copy location (line / line range when selecting) |
| `fuzzy_find` | Open file search overlay |
| `reload` | Reload |
| `cancel` | Cancel selection / close |
| `next_file` | Open next file (code = all files; diff = changed files) |
| `prev_file` | Open previous file (code = all files; diff = changed files) |
| `toggle_split` | Toggle unified ⇄ side-by-side diff |
| `grep` | Project-wide content search |

Use `"none"` to disable a key binding.
The `g` prefix for `gg` / `gd` cannot be remapped.

---

## Supported Languages

### Syntax Highlighting

**70+ languages** via inkjet (tree-sitter based).
**Markdown** is supported via tree-sitter's block + inline grammars, and code
inside fenced blocks (```rust, etc.) is highlighted per language when supported.

### Code Jump (`gd`) and Outline

Supported for **Rust, Python, JavaScript, Go, Ruby, C** only.

---

## Tech Stack

| Role | Crate |
|------|-------|
| TUI / terminal abstraction | `ratatui` + `crossterm` |
| Syntax highlighting | `inkjet` (tree-sitter based, 70+ languages) |
| Fuzzy matching | `nucleo-matcher` |
| File traversal (gitignore-aware) | `ignore` |
| Git diff / status | `git2` (vendored libgit2) |
| Symbol index / definition jump | `tree-sitter-tags` |
| Clipboard | `arboard` |
| Keymap config | `toml` |

---

## Known Limitations

- **No horizontal scroll** — long lines can be traversed with the cursor, but the view does not pan horizontally.
- **Side-by-side view** gives each pane about half the screen width, so long lines clip sooner. Toggling with `s` re-shows a nearby position rather than an exact match.
- **`gd` index builds in the background** — it starts on launch, so on large projects an early `gd` may briefly show "indexing…" (jumps are instant once ready).
- **Wide characters and tabs** — cursor and selection highlight positions may be slightly off on lines containing full-width characters or tab characters.
- **In-file search highlights whole lines** — the exact match position within the line is not highlighted.
- **Clipboard via arboard is for local use** — when connecting over SSH, clipboard content may not reach the remote terminal (consider OSC52 for SSH use cases).

---

## License

This project is dual-licensed under **MIT OR Apache-2.0**.
See [`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE) for details.

Third-party component licenses are listed in [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
Notable entries:

- **libgit2** (vendored static link via the git2 crate): GPLv2 + linking exception
- **nucleo-matcher**: MPL-2.0
