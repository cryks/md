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

    pager::run(args.file, source)
}
