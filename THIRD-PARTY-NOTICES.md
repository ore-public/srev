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

## Bundled grammars and themes

### inkjet (MIT OR Apache-2.0)

`inkjet` bundles many tree-sitter grammars and a collection of Helix editor
themes. `srev` uses the bundled `ONEDARK` theme, which originates from the Helix
editor repository (author: Gokul Soumya) and is re-distributed by inkjet
under `MIT OR Apache-2.0`. Other bundled themes carry their original authors'
licenses (mostly MIT) and inkjet ships per-theme `.LICENSE` files. Some
bundled highlight queries also originate from the Helix editor (MPL-2.0);
these are vendored and re-distributed by inkjet.

Grammars for the languages used by code navigation (Rust, C, Go, Python,
JavaScript, Ruby) and for Markdown (`tree-sitter-md`, block + inline) are
also pulled directly via the respective `tree-sitter-*` crates, which are
MIT licensed.

## Unicode data

### unicode-ident — (MIT OR Apache-2.0) AND Unicode-3.0

Contains data derived from the Unicode Character Database.

> Copyright © Unicode, Inc. Distributed under the Unicode License.

## Permissive Rust dependencies

The remaining dependencies are permissively licensed (MIT, Apache-2.0,
MIT OR Apache-2.0, Unlicense OR MIT, Apache-2.0 OR BSL-1.0, Zlib, etc.),
including but not limited to: `ratatui`, `crossterm`, `tree-sitter` and the
`tree-sitter-*` grammar crates, `tree-sitter-tags`, `ignore`, `walkdir`,
`aho-corasick`, `memchr`, `arboard`, `toml`, `clap`, `anyhow`.

Run `cargo deny check licenses` to validate the full dependency tree against
the policy in `deny.toml`.
