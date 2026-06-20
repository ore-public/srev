# srev — source reviewer

[日本語](README.ja.md) | **English**

A terminal TUI viewer specialized for *reading* code without opening an editor.
It keeps the controls simple so you can focus on reviewing diffs and browsing full files. No editing features.

---

## Features

- **Working tree vs HEAD diff** on demand with `d` — review uncommitted changes
- **Toggle diff ⇄ full code** with `d`, preserving the corresponding line position
- **Vim-like cursor** for reading code; **jump to definition** under cursor with `gd`
- **Find references** with `gr` — list usages of the symbol under the cursor across the project and jump to one
- **LSP-powered `gd` / `gr`** — when a language server is available (rust-analyzer / intelephense / ruby-lsp by default, configurable), jumps use real semantic analysis. Falls back automatically to a built-in tree-sitter tag index when no server is configured or running
- **Jump history** — every jump (`gd`, outline, project search, `gg`/`G`, in-file search) remembers where you came from; go back/forward with `(` / `)` (works across files). A **jump-history pane on the right** visualizes the trail with the current position highlighted (toggle with `J`)
- **Visual mode** (`v`/`V`) to select a range, then `y` to copy to clipboard
- **Outline pane** (bottom-left) lists symbols in the open file for quick navigation — from the **LSP `documentSymbol`** when a server is available (e.g. PHP via intelephense), otherwise from the tree-sitter index; the title shows which (`Symbols · LSP` / `Symbols · tree-sitter`)
- **In-file search** (`/` → `Enter`, `n`/`N` for next/previous match)
- **Fuzzy file search** (`Ctrl-P`, powered by nucleo — fzf equivalent)
- **Project-wide content search** (`Ctrl-F`, case-insensitive substring) — jump to a matching line
- **Inline fuzzy filtering** with `/` in tree, outline, and overlay panels
- **`Ctrl-R` pull + reload** — pulls the current branch from `origin` (its same-named remote branch) and re-reads the open file, git state, tree, and index while keeping cursor position. Falls back to a plain local reload when there's no upstream / you're offline; conflicts abort cleanly
- **Merge picker** (`m`) — fetches `origin`, then pick any branch (local or remote) with type-to-filter fuzzy search to merge into the current branch. On conflict the merge is aborted automatically and you're told to resolve it in your editor (srev has no editing)
- **Syntax highlighting** for 200+ languages via syntect (incl. PHP, HAML, TOML, TypeScript…) — **Markdown also highlights code inside fenced blocks** per language
- **Diff review navigation** — jump between change blocks with `n`/`N` or `}`/`{`, between changed files with `]`/`[`, and scroll long lines horizontally with `h`/`l` (`0`/`$` for start / far right). A **change overview bar** on the right edge maps where the edits are and highlights your current viewport. Diffs default to **side-by-side** (new/deleted files fall back to single column automatically); toggle with `s`. Within paired changed lines, **only the characters that actually differ are emphasized** with a brighter highlight over the row color (multibyte-safe; works in both side-by-side and unified)
- **Commit view** (`c`) — the left pane lists commits that differ from the default branch (`default..HEAD`), or the full log when you're on the default branch. Pick a commit to see its changed files (bottom-left) and the selected file's side-by-side diff
- **PR diff view** (`C`) — review the whole branch as one changeset: the left pane lists every file that differs from the default branch (three-dot `merge-base(default, HEAD)..HEAD`, so commits that landed on the default branch after you branched are excluded), and the right shows each file's full-context diff. `}`/`{` and `n`/`N` step through the changes
- **Branch switching** (`Ctrl-V`) — fetches `origin` first, then shows local and remote branches with **type-to-filter** fuzzy search; selecting one checks it out (via `git`, auto-creating a tracking branch for remotes) and reloads automatically. The **current branch** is always shown as a chip at the bottom-right of the status bar
- File tree shows change status (`M`=modified / `A`=added / `D`=deleted / `?`=untracked). Dotfiles and `.gitignore`d files are listed too, shown **dimmed**; ignored directories (e.g. `target/`) appear but aren't descended into, and the `.git` directory is excluded
- **Editor-style change gutter in code view** — added (green) / modified (blue) / deletion-above (red) vs HEAD; jump between change blocks with `}`/`{` (or `n`/`N` when no search is active), and a matching overview bar marks them on the right edge
- Single binary. Builds on Linux / macOS / Windows

---

## Installation

You need a Rust toolchain (`cargo`) and a **C compiler** (required to build
libgit2 and tree-sitter grammars). The release binary is about 10 MB.

### From crates.io (recommended)

```sh
cargo install srev
```

### From GitHub (latest development version)

```sh
cargo install --git https://github.com/ore-public/srev
```

Either way the binary is installed to `~/.cargo/bin/srev` (run it as `srev` if
that directory is on your `$PATH`).

### Build from source (for development)

```sh
git clone https://github.com/ore-public/srev
cd srev
cargo build --release   # output: target/release/srev
cargo test              # run unit tests
```

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
| `c` | Toggle the commit view (commits vs the default branch, or full log) |
| `C` | Toggle the PR diff view (all files changed vs the default branch, three-dot diff) |
| `Ctrl-V` | Switch branch (fetches `origin` first, then pick a branch to check out) |
| `m` | Merge picker — fetches `origin`, then pick a branch (local or remote) to merge into the current branch |
| `Ctrl-P` | Open fuzzy file search overlay |
| `Ctrl-F` | Project-wide content search (substring); `Enter` jumps to the matching line |
| `Ctrl-R` | Pull the current branch from `origin` (same-named branch), then reload. Falls back to a plain local reload when there's no upstream or you're offline; on conflict the merge is aborted |
| `]` / `[` | Open next / previous file (code mode = all files by path; diff mode = changed files) |
| `(` / `)` | Jump history: go back / forward through jumps (`Ctrl-O` also goes back) |
| `J` | Toggle the right-side jump-history pane (shown by default) |

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
| `gr` | List references (call sites) of word under cursor; `Enter` jumps (`↑`/`↓` or `Ctrl-n`/`Ctrl-p` to move, `Esc` to close) |
| `v` / `V` | Start visual mode (character / line) |
| `y` | Copy selection to clipboard |
| `Y` | Copy location to clipboard (no selection = `path:line:col`; single-line = `path:line`; multi-line = `path:start-end`) |
| `Esc` | Cancel selection |
| `/` → type → `Enter` | In-file search |
| `n` / `N` | Next / previous search match; with no active search, jump to next / previous change block |
| `}` / `{` | Jump to next / previous change block (vs HEAD) |

> The `g` prefix for `gg`, `gd` and `gr` is fixed and cannot be remapped in the config file.

#### Content pane — diff mode

| Key | Action |
|-----|--------|
| `j` / `k` | Scroll |
| `h` / `l`, `0` / `$` | Scroll horizontally for long lines (`0` = start, `$` = far right) |
| `gg` / `G` | Jump to start / end |
| `PageDown` / `PageUp` | Page scroll |
| `n` / `N`, `}` / `{` | Jump to next / previous change block |
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
| `goto_references` | List references (call sites) of the symbol under cursor |
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
| `jump_back` | Jump history: go back |
| `jump_forward` | Jump history: go forward |
| `toggle_jumps` | Toggle the jump-history pane |
| `toggle_commits` | Toggle the commit view |
| `toggle_branch_diff` | Toggle the PR diff view (vs the default branch) |
| `next_change` | Jump to the next change block |
| `prev_change` | Jump to the previous change block |
| `switch_branch` | Open the branch picker (fetch + checkout) |
| `merge_branch` | Open the merge picker (fetch + merge a branch into the current one) |

Use `"none"` to disable a key binding.
The `g` prefix for `gg` / `gd` cannot be remapped.

### Language Servers (LSP)

`gd` / `gr` use a language server when one is available, and otherwise fall back
to the tree-sitter tag index (see [Code Jump](#code-jump-gd--gr-and-outline)).

**Built-in defaults** (used out of the box if the binary is on your `$PATH`):

| Language | Extensions | Default command | Tag fallback |
|----------|------------|-----------------|--------------|
| Rust | `rs` | `rust-analyzer` | yes |
| PHP | `php`, `phtml` | `intelephense --stdio` | **no** |
| Ruby | `rb`, `rake`, `gemspec` | `ruby-lsp` | yes |

> PHP has no tag fallback, so `gd` / `gr` for PHP require a running server.
> Install your server of choice and make sure it is on `$PATH` (e.g.
> `rustup component add rust-analyzer`, `npm i -g intelephense`, `gem install ruby-lsp`).

**Configuration.** Add `[lsp.<id>]` tables to `~/.config/srev/config.toml`
(shared with the keymap config) and/or a repo-local `<project-root>/.srev.toml`,
which takes precedence. Each entry sets the file `extensions` and the launch
`command`. You can override a default, add a brand-new language **without
recompiling**, or disable one with an empty command.

```toml
# Override the Ruby server
[lsp.ruby]
extensions = ["rb", "rake", "gemspec"]
command = ["solargraph", "stdio"]   # instead of the default ruby-lsp

# Use an alternative PHP server
[lsp.php]
extensions = ["php", "phtml"]
command = ["phpactor", "language-server"]

# Add a language that isn't built in (no rebuild needed)
[lsp.go]
extensions = ["go"]
command = ["gopls"]

# Disable Rust LSP (fall back to the tag index)
[lsp.rust]
command = []
```

Notes:

- srev is **read-only**, so it only sends `textDocument/didOpen` — your files are
  never modified by the server.
- Servers are spawned lazily on the first `gd` / `gr` for a language and are
  terminated when srev exits.
- Right after opening a file, the first `gd` may fall back to the tag index while
  the server is still indexing — press it again once the server is ready.

---

## Supported Languages

### Syntax Highlighting

**200+ languages** via [syntect](https://github.com/trishume/syntect) with the
extended set from [two-face](https://github.com/CosmicHorrorDev/two-face),
including PHP, **HAML**, TOML, TypeScript, Vue, Svelte, and more. `.blade.php`
and `.phtml` are highlighted as PHP (a dedicated Blade grammar is not bundled).
**Markdown** highlights code inside fenced blocks (```rust, etc.) per language.

### Code Jump (`gd` / `gr`) and Outline

`gd` / `gr` resolve in two layers:

1. **LSP (preferred)** — if a language server is installed and on your `$PATH`,
   jumps use real semantic analysis (accurate across same-named symbols, imports,
   and scopes). See [Language Servers (LSP)](#language-servers-lsp) for the
   built-in defaults and how to configure them.
2. **tree-sitter tag index (fallback)** — used when no server is configured or
   while a server is still starting up. Name-based and approximate, supported for
   **Rust, Python, JavaScript, Go, Ruby, C**.

The **Outline pane** (Symbols) works the same way: it shows tree-sitter symbols
immediately on open, then **upgrades to the language server's `documentSymbol`**
once it is ready (so PHP and other non-tree-sitter languages get an outline too).
The pane title shows which source is active — `Symbols · tree-sitter` or
`Symbols · LSP`.

> Because the outline uses the server, opening a file of an LSP-configured
> language now **starts that server in the background** (previously it started
> on the first `gd` / `gr`). This also warms up `gd` / `gr`. Pure browsing of a
> Rust file therefore launches rust-analyzer; if no server is configured the
> outline stays on tree-sitter.

---

## Tech Stack

| Role | Crate |
|------|-------|
| TUI / terminal abstraction | `ratatui` + `crossterm` |
| Syntax highlighting | `syntect` + `two-face` (200+ languages) |
| Fuzzy matching | `nucleo-matcher` |
| File traversal (gitignore-aware) | `ignore` |
| Git diff / status | `git2` (vendored libgit2) |
| Symbol index / definition jump (fallback) | `tree-sitter-tags` |
| LSP client for `gd` / `gr` | `lsp-types` + `serde_json` (JSON-RPC over stdio) |
| Clipboard | `arboard` |
| Config (keymap + LSP) | `toml` |

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
