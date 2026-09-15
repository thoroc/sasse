//! Telling someone when an entry settles.
//!
//! The reasoning is in `docs/adr/on-settle-hook.md`. In short: a branch that is
//! queued and then evicted looks, from outside, exactly like one still waiting,
//! because neither has landed. The hook removes that ambiguity.
//!
//! Behind a trait so the worker's behaviour can be tested without spawning
//! anything, and so a hook that fails or hangs can be arranged exactly.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use eyre::{Result, WrapErr};

use crate::queue::store::Settled;

/// A hook is bounded rather than trusted.
///
/// This matters more than the exit code. A hook that curls a URL with no timeout
/// of its own, or that prompts, would otherwise block the tick indefinitely and
/// wedge the queue far more thoroughly than any missed notification. Ten seconds
/// rather than a setting: a notifier needing longer is doing something that
/// should not be inline.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// What became of one hook invocation.
///
/// A failure is reported rather than returned as an error, because the entry has
/// already settled and nothing about it is undone by a notifier misbehaving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Delivered,
    /// The hook ran and exited non-zero. Its output is carried so the failure
    /// can explain itself.
    Failed {
        exit_code: Option<i32>,
        output: String,
    },
    /// The hook was still running at the deadline and was killed.
    TimedOut,
    /// The hook could not be started at all.
    Unrunnable(String),
}

impl Delivery {
    pub fn delivered(&self) -> bool {
        matches!(self, Self::Delivered)
    }

    /// One line, for a tick to report.
    pub fn describe(&self) -> String {
        match self {
            Self::Delivered => "delivered".to_string(),
            Self::Failed { exit_code, output } => {
                let code = exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                let trimmed = output.trim();
                if trimmed.is_empty() {
                    format!("exited {code} with no output")
                } else {
                    format!("exited {code}: {trimmed}")
                }
            }
            Self::TimedOut => format!("still running after {}s, killed", TIMEOUT.as_secs()),
            Self::Unrunnable(why) => format!("could not be started: {why}"),
        }
    }
}

pub trait Notifier {
    /// Run `command` for one settled entry.
    fn settled(
        &self,
        command: &str,
        event: &Settled,
        repo_path: &str,
        base_branch: &str,
    ) -> Delivery;
}

/// Runs the hook through a shell, in the repository.
pub struct ShellNotifier {
    repo: PathBuf,
    timeout: Duration,
}

impl ShellNotifier {
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            timeout: TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn run(
        &self,
        command: &str,
        event: &Settled,
        repo_path: &str,
        base_branch: &str,
    ) -> Result<Delivery> {
        // Through a shell for the same reason as the gate: a hook written by
        // hand in a config file will contain pipes and quoting. `exec 2>&1`
        // merges its streams so a failure's explanation is not split in two.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("exec 2>&1\n{command}"))
            .current_dir(&self.repo)
            .env("SASSE_REPO", repo_path)
            .env("SASSE_BASE", base_branch)
            .env("SASSE_BRANCH", &event.branch)
            .env("SASSE_OUTCOME", event.state.as_str())
            .env("SASSE_ENTRY", event.entry.to_string())
            .env("SASSE_ATTEMPTS", event.attempts.to_string())
            .env("SASSE_REASON", event.reason.clone().unwrap_or_default())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .wrap_err("starting the on_settle hook")?;

        // Drained on another thread so a hook that writes more than a pipe
        // buffer cannot deadlock against our own wait.
        let stream = child.stdout.take().expect("stdout was piped");
        let drain = std::thread::spawn(move || {
            let mut collected = String::new();
            let mut stream = stream;
            let _ = stream.read_to_string(&mut collected);
            collected
        });

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = drain.join();
                return Ok(Delivery::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(25));
        };

        let output = drain.join().unwrap_or_default();

        if status.success() {
            Ok(Delivery::Delivered)
        } else {
            Ok(Delivery::Failed {
                exit_code: status.code(),
                output,
            })
        }
    }
}

impl Notifier for ShellNotifier {
    fn settled(
        &self,
        command: &str,
        event: &Settled,
        repo_path: &str,
        base_branch: &str,
    ) -> Delivery {
        match self.run(command, event, repo_path, base_branch) {
            Ok(delivery) => delivery,
            // Never propagated: the entry has already settled, and a notifier
            // that cannot be started is not a reason to fail the tick.
            Err(why) => Delivery::Unrunnable(format!("{why}")),
        }
    }
}

/// A notifier that does nothing, for when no hook is configured.
pub struct Silent;

impl Notifier for Silent {
    fn settled(&self, _: &str, _: &Settled, _: &str, _: &str) -> Delivery {
        Delivery::Delivered
    }
}

#[cfg(test)]
pub mod fake {
    use std::sync::Mutex;

    use super::*;

    /// Records what it was asked to announce, and answers with whatever was set.
    pub struct FakeNotifier {
        answer: Delivery,
        seen: Mutex<Vec<Settled>>,
    }

    impl FakeNotifier {
        pub fn working() -> Self {
            Self {
                answer: Delivery::Delivered,
                seen: Mutex::new(Vec::new()),
            }
        }

        pub fn answering(answer: Delivery) -> Self {
            Self {
                answer,
                seen: Mutex::new(Vec::new()),
            }
        }

        pub fn seen(&self) -> Vec<Settled> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Notifier for FakeNotifier {
        fn settled(&self, _: &str, event: &Settled, _: &str, _: &str) -> Delivery {
            self.seen.lock().unwrap().push(event.clone());
            self.answer.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::EntryState;

    fn merged() -> Settled {
        Settled {
            entry: 7,
            branch: "feat/x".into(),
            state: EntryState::Merged,
            attempts: 0,
            reason: None,
        }
    }

    fn evicted() -> Settled {
        Settled {
            entry: 9,
            branch: "feat/bad".into(),
            state: EntryState::Evicted,
            attempts: 3,
            reason: Some("failed the gate on its own".into()),
        }
    }

    fn here() -> ShellNotifier {
        ShellNotifier::new(std::env::temp_dir())
    }

    #[test]
    fn a_working_hook_delivers() {
        assert_eq!(
            here().settled("true", &merged(), "/repo", "main"),
            Delivery::Delivered
        );
    }

    #[test]
    fn a_failing_hook_reports_its_code_and_output() {
        let delivery = here().settled("echo went wrong; exit 4", &merged(), "/repo", "main");
        match &delivery {
            Delivery::Failed { exit_code, output } => {
                assert_eq!(*exit_code, Some(4));
                assert!(output.contains("went wrong"), "got {output:?}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(delivery.describe().contains("went wrong"));
    }

    /// The important half of the failure handling: a hook that never exits must
    /// not hold the tick open.
    #[test]
    fn a_hanging_hook_is_killed_at_the_deadline() {
        let notifier = here().with_timeout(Duration::from_millis(200));
        let began = Instant::now();

        let delivery = notifier.settled("sleep 30", &merged(), "/repo", "main");

        assert_eq!(delivery, Delivery::TimedOut);
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "waited {:?}, so the deadline did not hold",
            began.elapsed()
        );
    }

    #[test]
    fn the_entry_is_described_in_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let notifier = ShellNotifier::new(dir.path());

        let delivery = notifier.settled(
            "printf '%s|%s|%s|%s|%s|%s\\n' \
             \"$SASSE_BRANCH\" \"$SASSE_OUTCOME\" \"$SASSE_ENTRY\" \
             \"$SASSE_ATTEMPTS\" \"$SASSE_REASON\" \"$SASSE_BASE\" > recorded; exit 1",
            &evicted(),
            "/repo",
            "release",
        );
        assert!(matches!(delivery, Delivery::Failed { .. }));

        let recorded = std::fs::read_to_string(dir.path().join("recorded")).unwrap();
        assert_eq!(
            recorded.trim(),
            "feat/bad|evicted|9|3|failed the gate on its own|release"
        );
    }

    #[test]
    fn a_merged_entry_carries_an_empty_reason() {
        let dir = tempfile::tempdir().unwrap();
        let notifier = ShellNotifier::new(dir.path());

        notifier.settled(
            "printf '[%s][%s]' \"$SASSE_OUTCOME\" \"$SASSE_REASON\" > recorded",
            &merged(),
            "/repo",
            "main",
        );

        assert_eq!(
            std::fs::read_to_string(dir.path().join("recorded")).unwrap(),
            "[merged][]"
        );
    }

    #[test]
    fn a_hook_that_cannot_run_is_reported_rather_than_raised() {
        let notifier = ShellNotifier::new("/definitely/not/a/directory");
        let delivery = notifier.settled("true", &merged(), "/repo", "main");

        assert!(
            matches!(delivery, Delivery::Unrunnable(_)),
            "got {delivery:?}"
        );
        assert!(delivery.describe().contains("could not be started"));
    }

    /// A hook writing more than a pipe buffer must not deadlock against the wait.
    #[test]
    fn a_chatty_hook_does_not_deadlock() {
        let delivery = here().settled(
            "for i in $(seq 1 20000); do echo 'a line of output'; done; exit 2",
            &merged(),
            "/repo",
            "main",
        );
        assert!(matches!(
            delivery,
            Delivery::Failed {
                exit_code: Some(2),
                ..
            }
        ));
    }

    #[test]
    fn the_silent_notifier_runs_nothing_and_says_so() {
        assert!(
            Silent
                .settled("true", &merged(), "/repo", "main")
                .delivered()
        );
    }
}
