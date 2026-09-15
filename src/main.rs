use std::cell::Cell;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use eyre::Result;

use sasse::config::{self, Config};
use sasse::git::{CommandGit, Git, Sha};
use sasse::queue::store::{Amendment, Run, Snapshot, StatusEntry};
use sasse::queue::{lease, store};
use sasse::worker::{Progress, TickOutcome, WorkOptions, Worker, work};
use sasse::{db, gate, shutdown};

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

#[derive(Args)]
struct Integration {
    /// The checkout candidates are assembled and gated in. It must be a
    /// detached worktree of the repository, and must not have the base branch
    /// checked out.
    #[arg(long)]
    integration: PathBuf,

    /// Where gate output is kept. Logs outlive the queue rows that point at
    /// them.
    #[arg(long, default_value = "sasse-logs")]
    logs: PathBuf,
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

        #[command(flatten)]
        integration: Integration,
    },

    /// Keep ticking until interrupted.
    Work {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        integration: Integration,

        /// How long to wait when there is nothing to do.
        #[arg(long, default_value = "2", value_name = "SECONDS")]
        interval: u64,

        /// Stop after this many ticks fail in a row.
        #[arg(long, default_value = "5")]
        give_up_after: usize,
    },

    /// Take a waiting entry out of the queue.
    Dequeue {
        /// Entry number, as shown by `sasse status`.
        entry: i64,

        #[command(flatten)]
        target: Target,
    },

    /// Move a waiting entry to the front of the queue.
    Promote {
        /// Entry number, as shown by `sasse status`.
        entry: i64,

        #[command(flatten)]
        target: Target,
    },

    /// Show gate output.
    Logs {
        /// Candidate to show. Omit to list recent gate runs.
        candidate: Option<i64>,

        #[command(flatten)]
        target: Target,

        /// Lines of the log to show.
        #[arg(long, default_value = "40")]
        tail: usize,
    },

    /// Show the queue.
    Status {
        #[command(flatten)]
        target: Target,

        /// How many settled entries to list.
        #[arg(long, default_value = "5")]
        settled: usize,
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
            // Queueing only reads refs, so the integration checkout is
            // irrelevant here.
            let git = CommandGit::new(&repo, &repo);
            let sha = git.resolve(&branch)?;

            let id = store::enqueue(&conn, &repo, &target.base, &branch, &sha)?;
            println!("queued {branch} at {sha} as entry {id}");
        }

        Command::Tick {
            target,
            integration,
        } => {
            // An interrupt kills the gate's shell along with this process, and
            // that must not be recorded as the branch's fault.
            shutdown::install()?;

            let mut conn = db::open(&cli.db)?;
            let repo = canonical(&target.repo)?;
            let git = CommandGit::new(&repo, &integration.integration);
            let shell = gate::ShellGate::new(&integration.integration);

            let outcome = Worker::new(
                &mut conn,
                &git,
                &shell,
                &repo,
                &target.base,
                &integration.logs,
            )
            .with_interrupt(&shutdown::requested)
            .tick()?;
            println!("{}", describe(&outcome));
        }

        Command::Work {
            target,
            integration,
            interval,
            give_up_after,
        } => {
            // Installed before the first tick, so an interrupt during the very
            // first gate still stops cleanly rather than killing it midway.
            shutdown::install()?;

            let mut conn = db::open(&cli.db)?;
            let repo = canonical(&target.repo)?;
            let git = CommandGit::new(&repo, &integration.integration);
            let shell = gate::ShellGate::new(&integration.integration);

            println!("working {} on {}; interrupt to stop", target.base, repo);

            // A resident worker that logs "nothing queued" every interval
            // buries the lines that matter, so an unchanging quiet state is
            // reported once and then left alone.
            let repeating = Cell::new(false);
            let summary = work(
                &mut conn,
                &git,
                &shell,
                &repo,
                &target.base,
                &integration.logs,
                WorkOptions {
                    idle: Duration::from_secs(interval),
                    give_up_after,
                },
                &shutdown::requested,
                &|progress| match progress {
                    Progress::Ticked(outcome) => {
                        let quiet = !outcome.is_progress();
                        if !(quiet && repeating.get()) {
                            println!("{}", describe(outcome));
                        }
                        repeating.set(quiet);
                    }
                    Progress::Failed(failed) => {
                        eprintln!("tick failed: {failed}");
                        repeating.set(false);
                    }
                },
            )?;

            println!(
                "stopped after {} tick(s): {} landed, {} evicted, {} failed tick(s)",
                summary.ticks, summary.entries_landed, summary.entries_evicted, summary.failures
            );
        }

        Command::Dequeue { entry, target } => {
            let conn = db::open(&cli.db)?;
            let repo = canonical(&target.repo)?;

            match store::dequeue(&conn, &repo, &target.base, entry)? {
                Amendment::Applied => println!("entry {entry} taken out of the queue"),
                Amendment::NotWaiting(state) => println!(
                    "entry {entry} is {}, not waiting, so it was left alone",
                    state.as_str()
                ),
                Amendment::Unknown => {
                    println!("no entry {entry} queued against {}", target.base)
                }
            }
        }

        Command::Promote { entry, target } => {
            let conn = db::open(&cli.db)?;
            let repo = canonical(&target.repo)?;

            match store::promote(&conn, &repo, &target.base, entry)? {
                Amendment::Applied => println!("entry {entry} moved to the front of the queue"),
                Amendment::NotWaiting(state) => println!(
                    "entry {entry} is {}, not waiting, so it was left alone",
                    state.as_str()
                ),
                Amendment::Unknown => {
                    println!("no entry {entry} queued against {}", target.base)
                }
            }
        }

        Command::Logs {
            candidate,
            target,
            tail,
        } => {
            let conn = db::open(&cli.db)?;
            let repo = canonical(&target.repo)?;

            match candidate {
                None => {
                    let recent = store::recent_runs(&conn, &repo, &target.base, 10)?;
                    if recent.is_empty() {
                        println!("no gate has run against {} yet", target.base);
                    } else {
                        print_runs(&recent);
                        println!();
                        println!("sasse logs <candidate> to read one");
                    }
                }
                Some(candidate) => {
                    let runs = store::runs(&conn, candidate)?;
                    if runs.is_empty() {
                        println!("candidate {candidate} has no gate runs");
                    } else {
                        print_runs(&runs);
                        print_tail(runs.last().expect("checked not empty"), tail);
                    }
                }
            }
        }

        Command::Status { target, settled } => {
            let conn = db::open(&cli.db)?;
            let repo = canonical(&target.repo)?;
            let git = CommandGit::new(&repo, &repo);

            let snapshot = store::snapshot(&conn, &repo, &target.base, settled)?;
            let held = lease::current(&conn, &repo, &target.base)?;
            let tip = git.resolve(&target.base).ok();
            // The retry budget makes an attempt count meaningful, but status
            // should still work when the config cannot be read.
            let budget = tip.as_ref().and_then(|at| gate_budget(&git, at));

            print_status(&repo, &target.base, tip.as_ref(), held, budget, &snapshot);
        }
    }

    Ok(())
}

/// Repository paths are part of an entry's identity, so `enqueue` run from one
/// directory and `tick` from another must agree on the spelling.
fn canonical(path: &std::path::Path) -> Result<String> {
    Ok(path.canonicalize()?.to_string_lossy().into_owned())
}

fn gate_budget(git: &CommandGit, at: &Sha) -> Option<u32> {
    let source = git.read_file_at(at, config::CONFIG_PATH).ok()??;
    Config::parse(&source)
        .ok()
        .map(|config| config.max_attempts)
}

fn describe(outcome: &TickOutcome) -> String {
    match outcome {
        TickOutcome::Idle => "nothing queued".to_string(),

        TickOutcome::AnotherWorkerHolds { holder_pid } => {
            format!("another worker holds this branch (pid {holder_pid})")
        }

        TickOutcome::BaseCheckedOut { holders } => {
            let mut lines =
                vec!["refusing to touch the base branch: it is checked out in".to_string()];
            for holder in holders {
                lines.push(format!("  {}", holder.display()));
            }
            lines.push("git would move it anyway and leave that checkout inconsistent".to_string());
            lines.join("\n")
        }

        TickOutcome::Landed {
            candidate,
            entries,
            at,
        } => format!(
            "candidate {candidate} passed; {entries} entr(ies) landed at {}",
            short(at)
        ),

        TickOutcome::Abandoned {
            candidate,
            requeued,
            reason,
        } => format!(
            "candidate {candidate} discarded ({reason:?}); {requeued} requeued, no attempts spent"
        ),

        TickOutcome::Blamed {
            candidate,
            branch,
            blame,
            reason,
            ..
        } => format!("candidate {candidate} failed: {branch} {reason:?}, {blame:?}"),

        TickOutcome::Split {
            failed,
            next,
            retrying,
        } => format!("candidate {failed} failed; bisecting into candidate {next} of {retrying}"),
    }
}

fn print_status(
    repo: &str,
    base: &str,
    tip: Option<&Sha>,
    held: Option<lease::Held>,
    budget: Option<u32>,
    snapshot: &Snapshot,
) {
    let at = tip.map(short).unwrap_or_else(|| "unresolved".to_string());
    println!("repo    {repo}");
    println!("base    {base} @ {at}");

    match held {
        None => println!("worker  none"),
        Some(held) => {
            let mut notes = Vec::new();
            if !held.holder_present {
                notes.push("process gone".to_string());
            }
            notes.push(if held.expired {
                format!("expired {} ago", ago(held.expires_at))
            } else {
                format!("expires in {}", until(held.expires_at))
            });
            println!("worker  pid {} ({})", held.holder_pid, notes.join(", "));
        }
    }

    if let Some(candidate) = &snapshot.active {
        println!();
        println!(
            "candidate {} on base {} ({} entr(ies))",
            candidate.id,
            short(&candidate.base_sha),
            candidate.entries.len()
        );
        for (position, entry) in candidate.entries.iter().enumerate() {
            println!(
                "  {}. {} {}",
                position + 1,
                entry.branch,
                short(&entry.branch_sha)
            );
        }
    }

    print_entries("queued", &snapshot.queued, budget);
    print_entries("in a candidate", &snapshot.batched, budget);
    print_entries("settled", &snapshot.settled, budget);
}

fn print_entries(heading: &str, entries: &[StatusEntry], budget: Option<u32>) {
    if entries.is_empty() {
        return;
    }

    println!();
    println!("{heading} ({})", entries.len());
    for entry in entries {
        let mut notes = Vec::new();
        if entry.priority > 0 {
            notes.push("promoted".to_string());
        }
        if entry.attempts > 0 {
            notes.push(match budget {
                Some(budget) => format!("attempt {} of {budget}", entry.attempts),
                None => format!("{} attempt(s) spent", entry.attempts),
            });
        }
        if let Some(reason) = &entry.evict_reason {
            notes.push(reason.clone());
        }

        let suffix = if notes.is_empty() {
            String::new()
        } else {
            format!("  ({})", notes.join("; "))
        };
        println!(
            "  #{:<4} {:<10} {:<24} {}{}",
            entry.id,
            entry.state.as_str(),
            entry.branch,
            short(&entry.branch_sha),
            suffix
        );
    }
}

fn print_runs(runs: &[Run]) {
    for run in runs {
        let verdict = match run.exit_code {
            Some(0) => "passed".to_string(),
            Some(code) => format!("failed ({code})"),
            None => "killed".to_string(),
        };
        println!(
            "run {:<4} candidate {:<4} {:<12} {}  {}",
            run.id, run.candidate_id, verdict, run.started_at, run.command
        );
    }
}

fn print_tail(run: &Run, lines: usize) {
    let Some(path) = &run.log_path else {
        println!();
        println!("run {} recorded no log", run.id);
        return;
    };

    println!();
    println!("{path}");

    match tail_of(std::path::Path::new(path), lines) {
        Err(unreadable) => println!("  (could not be read: {unreadable})"),
        Ok(tail) if tail.is_empty() => println!("  (empty)"),
        Ok(tail) => {
            for line in tail {
                println!("  {line}");
            }
        }
    }
}

/// The last `lines` lines, without holding the whole file: a verbose gate can
/// leave a log far larger than the part anyone wants to read.
fn tail_of(path: &std::path::Path, lines: usize) -> Result<Vec<String>> {
    let file = File::open(path)?;
    let mut kept: VecDeque<String> = VecDeque::with_capacity(lines.saturating_add(1));

    for line in BufReader::new(file).lines() {
        kept.push_back(line?);
        if kept.len() > lines {
            kept.pop_front();
        }
    }

    Ok(kept.into())
}

fn short(sha: &Sha) -> String {
    sha.as_str().chars().take(7).collect()
}

fn until(when: DateTime<Utc>) -> String {
    humanise(when.signed_duration_since(Utc::now()))
}

fn ago(when: DateTime<Utc>) -> String {
    humanise(Utc::now().signed_duration_since(when))
}

fn humanise(span: chrono::TimeDelta) -> String {
    let seconds = span.num_seconds().max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
    }
}
