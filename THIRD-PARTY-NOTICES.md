# Third-party notices

`srev` is distributed as a single binary that statically links and bundles
third-party components. This file lists the notable licenses and attributions.
A full machine-generated inventory can be produced with
[`cargo-about`](https://github.com/EmbarkStudios/cargo-about) or
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny).

## Statically linked native library

### libgit2 (via `git2` / `libgit2-sys`, feature `vendored-libgit2`)

`srev` statically links libgit2. libgit2 is licensed under
**GPLv2 with a linking exception**, which explicitly permits linking the
compiled library into programs under any license and distributing those
combinations without restriction. There is **no source-disclosure obligation**
for `srev` itself.

Obligation: retain libgit2's copyright notice and a copy of its license
(GPLv2 + linking exception) in distributions.

> Copyright (C) the libgit2 contributors. See the libgit2 `COPYING` file.

## Weak-copyleft Rust crate

### nucleo-matcher — MPL-2.0

`nucleo-matcher` is licensed under the **Mozilla Public License 2.0**
(file-level weak copyleft). It does not affect the license of `srev`'s own
source. The source of `nucleo-matcher` is available on crates.io:
<https://crates.io/crates/nucleo-matcher>.

## Syntax highlighting (syntect + two-face)

Syntax highlighting uses `syntect` (MIT) together with the extended syntax and
theme set provided by `two-face` (MIT). `two-face` bundles Sublime-syntax
definitions and color themes collected by the `bat` project from various
Sublime Text packages; each retains its original (permissive, mostly MIT)
license. The default color theme is syntect's bundled `base16-ocean.dark`.

## Code-navigation grammars (tree-sitter)

Symbol extraction for `gd` / outline (Rust, C, Go, Python, JavaScript, Ruby)
uses `tree-sitter` with the respective `tree-sitter-*` grammar crates and
`tree-sitter-tags`, all MIT licensed.

## Unicode data

### unicode-ident — (MIT OR Apache-2.0) AND Unicode-3.0

Contains data derived from the Unicode Character Database.

> Copyright © Unicode, Inc. Distributed under the Unicode License.

## Permissive Rust dependencies

The remaining dependencies are permissively licensed (MIT, Apache-2.0,
MIT OR Apache-2.0, Unlicense OR MIT, Apache-2.0 OR BSL-1.0, Zlib, etc.),
including but not limited to: `ratatui`, `crossterm`, `syntect`, `two-face`,
`tree-sitter` and the `tree-sitter-*` grammar crates, `tree-sitter-tags`,
`ignore`, `walkdir`, `aho-corasick`, `memchr`, `arboard`, `toml`, `clap`,
`anyhow`.

Run `cargo deny check licenses` to validate the full dependency tree against
the policy in `deny.toml`.
