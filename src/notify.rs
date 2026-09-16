//! Telling someone when an entry settles.
//!
//! The reasoning is in `docs/adr/004-on-settle-hook.md`. In short: a branch that is
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

#[cfg(unix)]
use std::os::unix::process::CommandExt;

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
        let mut spawning = Command::new("/bin/sh");
        spawning
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
            .stderr(Stdio::null());

        // Its own process group, so the timeout can reach everything the hook
        // started and not just the shell. This is the lesson recorded in
        // docs/salvage-from-task-spooler.md: ts puts each job in a new process
        // group precisely so the whole tree can be signalled at once.
        #[cfg(unix)]
        spawning.process_group(0);

        let mut child = spawning.spawn().wrap_err("starting the on_settle hook")?;

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
                // The group, not just the shell. Killing only the shell can
                // leave a descendant holding the pipe's write end, and then the
                // drain below never sees end of file and the wait that was
                // supposed to be bounded runs for as long as the hook would
                // have.
                abandon(&mut child);
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

/// Kill a timed-out hook and everything it started.
#[cfg(unix)]
fn abandon(child: &mut std::process::Child) {
    let pid = child.id() as i32;
    // SAFETY: a negative pid addresses the process group, which this child was
    // given to itself. A group that has already gone is reported through errno
    // rather than being undefined.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn abandon(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
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

    /// The shell cannot exec-optimise a backgrounded command, so it forks. If
    /// only the shell is killed, the surviving child keeps the pipe's write end
    /// open and draining its output never sees end of file.
    #[test]
    fn a_hook_whose_child_outlives_the_shell_is_still_killed() {
        let notifier = here().with_timeout(Duration::from_millis(200));
        let began = Instant::now();

        let delivery = notifier.settled("sleep 30 & wait", &merged(), "/repo", "main");

        assert_eq!(delivery, Delivery::TimedOut);
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "waited {:?}, so a descendant was still holding the pipe",
            began.elapsed()
        );
    }

    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 runs the existence check without delivering anything.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// The timing assertions above would still pass if a descendant survived and
    /// merely stopped holding the pipe. This asserts the operational
    /// consequence: nothing the hook started outlives the kill.
    ///
    /// The child's pid is checked directly rather than waiting for a marker file
    /// that a surviving child would eventually write. An earlier version did the
    /// latter and was a race between the kill and the clock: under parallel test
    /// load the poll loop can be descheduled past the sleep it was betting
    /// against, and the test failed for reasons that had nothing to do with the
    /// behaviour.
    #[test]
    fn a_timed_out_hook_leaves_no_descendant_behind() {
        let dir = tempfile::tempdir().unwrap();
        let notifier = ShellNotifier::new(dir.path()).with_timeout(Duration::from_millis(500));

        // Backgrounded, so the shell forks, and the pid is recorded at once.
        let delivery = notifier.settled(
            "sleep 30 & echo $! > child.pid; wait",
            &merged(),
            "/repo",
            "main",
        );
        assert_eq!(delivery, Delivery::TimedOut);

        let recorded = std::fs::read_to_string(dir.path().join("child.pid"))
            .expect("the hook should record its child's pid long before the deadline");
        let child: i32 = recorded.trim().parse().expect("a pid");

        // A killed process is briefly a zombie until whatever inherits it reaps
        // it, so allow for that rather than asserting on the first observation.
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(child) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(
            !alive(child),
            "pid {child} was still alive three seconds after the group kill"
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
