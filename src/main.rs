use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

#[derive(Parser)]
#[command(
    name = "sasse",
    version,
    about = "A local merge queue: batch, gate, fast-forward"
)]
struct Cli {
    /// Queue database. Created on first use.
    #[arg(long, default_value = "sasse.db", global = true)]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create or upgrade the queue database, then exit.
    Migrate,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Migrate => {
            sasse::db::open(&cli.db)?;
            println!("queue database ready at {}", cli.db.display());
        }
    }

    Ok(())
}
