use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use eyre::Result;

use sasse::git::{CommandGit, Git};
use sasse::queue::store;
use sasse::worker::{TickOutcome, Worker};
use sasse::{db, gate};

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

#[derive(Args)]
struct Target {
    /// The repository whose base branch is being integrated into.
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// The branch entries are queued against.
    #[arg(long, default_value = "main")]
    base: String,
}

#[derive(Subcommand)]
enum Command {
    /// Create or upgrade the queue database, then exit.
    Migrate,

    /// Put a branch in the queue.
    Enqueue {
        /// The branch to integrate.
        branch: String,

        #[command(flatten)]
        target: Target,
    },

    /// Advance the queue by at most one candidate.
    Tick {
        #[command(flatten)]
        target: Target,

        /// The checkout candidates are assembled and gated in. It must be a
        /// detached worktree of the repository, and must not have the base
        /// branch checked out.
        #[arg(long)]
        integration: PathBuf,

        /// Where gate output is kept. Logs outlive the queue rows that point at
        /// them.
        #[arg(long, default_value = "sasse-logs")]
        logs: PathBuf,
    },
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Migrate => {
            db::open(&cli.db)?;
            println!("queue database ready at {}", cli.db.display());
        }

        Command::Enqueue { branch, target } => {
            let conn = db::open(&cli.db)?;
            let repo = canonical(&target.repo)?;
            // The integration checkout is irrelevant to queueing; only refs are
            // read.
            let git = CommandGit::new(&repo, &repo);
            let sha = git.resolve(&branch)?;

            let id = store::enqueue(&conn, &repo, &target.base, &branch, &sha)?;
            println!("queued {branch} at {sha} as entry {id}");
        }

        Command::Tick {
            target,
            integration,
            logs,
        } => {
            let mut conn = db::open(&cli.db)?;
            let repo = canonical(&target.repo)?;
            let git = CommandGit::new(&repo, &integration);
            let gate = gate::ShellGate::new(&integration);

            let outcome = Worker::new(&mut conn, &git, &gate, &repo, &target.base, &logs).tick()?;
            report(&outcome);
        }
    }

    Ok(())
}

/// Repository paths are the queue's identity for an entry, so they must not
/// differ between `enqueue` run from one directory and `tick` from another.
fn canonical(path: &std::path::Path) -> Result<String> {
    Ok(path.canonicalize()?.to_string_lossy().into_owned())
}

fn report(outcome: &TickOutcome) {
    match outcome {
        TickOutcome::Idle => println!("nothing queued"),

        TickOutcome::AnotherWorkerHolds { holder_pid } => {
            println!("another worker holds this branch (pid {holder_pid})");
        }

        TickOutcome::BaseCheckedOut { holders } => {
            println!("refusing to touch the base branch: it is checked out in");
            for holder in holders {
                println!("  {}", holder.display());
            }
            println!("git would move it anyway and leave that checkout inconsistent");
        }

        TickOutcome::Landed {
            candidate,
            entries,
            at,
        } => println!("candidate {candidate} passed; {entries} entr(ies) landed at {at}"),

        TickOutcome::Abandoned {
            candidate,
            requeued,
            reason,
        } => println!(
            "candidate {candidate} discarded ({reason:?}); {requeued} requeued, no attempts spent"
        ),

        TickOutcome::Blamed {
            candidate,
            branch,
            blame,
            reason,
            ..
        } => println!("candidate {candidate} failed: {branch} {reason:?}, {blame:?}"),

        TickOutcome::Split {
            failed,
            next,
            retrying,
        } => println!("candidate {failed} failed; bisecting into candidate {next} of {retrying}"),
    }
}
