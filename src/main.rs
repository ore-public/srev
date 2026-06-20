mod app;
mod diffview;
mod finder;
mod fuzzy;
mod git;
mod grep;
mod highlight;
mod keymap;
mod lsp;
mod tags;
mod tree;
mod ui;
mod width;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

/// srev — コードレビューに特化したターミナルビューワー
///
/// 作業ツリーの差分を見つつ、差分とフルソースを行き来しながら読むためのツール。
#[derive(Parser, Debug)]
#[command(name = "srev", version, about)]
struct Cli {
    /// 開くディレクトリ（省略時はカレントディレクトリ）
    #[arg(default_value = ".")]
    path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = cli.path.canonicalize().unwrap_or(cli.path);

    let mut terminal = ratatui::init();
    let result = app::App::new(root).run(&mut terminal);
    ratatui::restore();
    result
}
