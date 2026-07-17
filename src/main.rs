//! CLI 引数を解釈し、最初の Markdown 読み込みを pager へ渡す。
//! 端末状態と再読み込みのライフサイクルは `pager` が所有し、この層では扱わない。

mod diff;
mod inline;
mod pager;
mod renderer;
mod style;
mod table;

use std::{fs, path::PathBuf, process};

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "md", version, about = "View Markdown files in your terminal")]
struct Args {
    #[arg(short, long, help = "Reload the file when it changes")]
    watch: bool,
    file: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("md: {error:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let source = fs::read_to_string(&args.file)
        .with_context(|| format!("failed to read {}", args.file.display()))?;

    pager::run(args.file, source, args.watch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_watch_option() {
        let args = Args::try_parse_from(["md", "--watch", "notes.md"]).unwrap();

        assert!(args.watch);
        assert_eq!(args.file, PathBuf::from("notes.md"));

        let args = Args::try_parse_from(["md", "-w", "notes.md"]).unwrap();
        assert!(args.watch);
    }

    #[test]
    fn watch_is_disabled_by_default() {
        let args = Args::try_parse_from(["md", "notes.md"]).unwrap();

        assert!(!args.watch);
    }
}
