//! Running the gate against an assembled candidate.
//!
//! Behind a trait for the same reason as the git operations: so the worker's
//! decisions can be tested without waiting on a real test suite, and so a
//! failing gate can be arranged exactly rather than approximated.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use eyre::{Result, WrapErr};

use crate::git::Sha;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    /// `None` when the gate was killed by a signal rather than exiting.
    Failed {
        exit_code: Option<i32>,
    },
}

impl Verdict {
    pub fn passed(&self) -> bool {
        matches!(self, Self::Passed)
    }
}

pub trait Gate {
    /// Run `command` against the candidate currently checked out, writing its
    /// combined output to `log_path`.
    ///
    /// The candidate id is passed so the run can be identified in logs. The
    /// gate is not expected to use it to find the code: the code is whatever is
    /// checked out.
    fn run(&self, command: &str, candidate: &Sha, log_path: &Path) -> Result<Verdict>;
}

/// Runs the gate through a shell, in the integration checkout.
pub struct ShellGate {
    integration: PathBuf,
}

impl ShellGate {
    pub fn new(integration: impl Into<PathBuf>) -> Self {
        Self {
            integration: integration.into(),
        }
    }
}

impl Gate for ShellGate {
    fn run(&self, command: &str, candidate: &Sha, log_path: &Path) -> Result<Verdict> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating the log directory {}", parent.display()))?;
        }

        let log = File::create(log_path)
            .wrap_err_with(|| format!("creating the gate log {}", log_path.display()))?;
        let errors = log
            .try_clone()
            .wrap_err("duplicating the gate log handle for stderr")?;

        // Through a shell, because the gate is written by a human in a config
        // file and will contain pipes, &&, and shell quoting.
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.integration)
            .env("SASSE_CANDIDATE", candidate.as_str())
            // stdin closed: a gate that waits for input would hang the queue
            // with no indication of why.
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errors))
            .status()
            .wrap_err_with(|| format!("running the gate: {command}"))?;

        if status.success() {
            Ok(Verdict::Passed)
        } else {
            Ok(Verdict::Failed {
                exit_code: status.code(),
            })
        }
    }
}

#[cfg(test)]
pub mod fake {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// A gate whose verdicts are decided in advance.
    ///
    /// Verdicts are scripted rather than keyed on the candidate id because a
    /// candidate's id is synthesised while the batch is assembled, so a test
    /// cannot know it beforehand. Driving by call order lets a test lay out a
    /// whole bisection cascade up front.
    pub struct FakeGate {
        script: Mutex<VecDeque<Verdict>>,
        fallback: Verdict,
        calls: Mutex<Vec<(String, Sha)>>,
    }

    impl FakeGate {
        pub fn passing() -> Self {
            Self::always(Verdict::Passed)
        }

        pub fn failing() -> Self {
            Self::always(Verdict::Failed { exit_code: Some(1) })
        }

        fn always(fallback: Verdict) -> Self {
            Self {
                script: Mutex::new(VecDeque::new()),
                fallback,
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Answer these verdicts in order, then pass.
        pub fn scripted(verdicts: impl IntoIterator<Item = Verdict>) -> Self {
            Self {
                script: Mutex::new(verdicts.into_iter().collect()),
                fallback: Verdict::Passed,
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Every invocation so far, as (command, candidate).
        pub fn calls(&self) -> Vec<(String, Sha)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Gate for FakeGate {
        fn run(&self, command: &str, candidate: &Sha, _log_path: &Path) -> Result<Verdict> {
            self.calls
                .lock()
                .unwrap()
                .push((command.to_string(), candidate.clone()));
            Ok(self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.fallback.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> Sha {
        Sha::parse(&"ab".repeat(20)).unwrap()
    }

    #[test]
    fn a_succeeding_command_passes() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("logs/run.log");

        assert_eq!(
            gate.run("true", &candidate(), &log).unwrap(),
            Verdict::Passed
        );
    }

    #[test]
    fn a_failing_command_reports_its_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());

        assert_eq!(
            gate.run("exit 3", &candidate(), &dir.path().join("run.log"))
                .unwrap(),
            Verdict::Failed { exit_code: Some(3) }
        );
    }

    #[test]
    fn output_from_both_streams_is_captured() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");

        gate.run("echo out; echo err 1>&2", &candidate(), &log)
            .unwrap();

        let captured = std::fs::read_to_string(&log).unwrap();
        assert!(captured.contains("out"), "stdout missing from {captured:?}");
        assert!(captured.contains("err"), "stderr missing from {captured:?}");
    }

    #[test]
    fn the_log_directory_is_created_if_it_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("deep/nested/run.log");

        gate.run("true", &candidate(), &log).unwrap();
        assert!(log.exists());
    }

    #[test]
    fn the_gate_runs_in_the_integration_checkout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "here").unwrap();
        let gate = ShellGate::new(dir.path());

        assert_eq!(
            gate.run("test -f marker", &candidate(), &dir.path().join("run.log"))
                .unwrap(),
            Verdict::Passed
        );
    }

    /// A gate that reads stdin would otherwise block the queue with nothing to
    /// indicate why.
    #[test]
    fn stdin_is_closed_so_a_prompting_gate_cannot_hang() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");

        // Reading from a closed stdin returns end of file immediately.
        assert_eq!(
            gate.run("read line", &candidate(), &log).unwrap(),
            Verdict::Failed { exit_code: Some(1) }
        );
    }

    #[test]
    fn the_candidate_is_available_to_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");
        let c = candidate();

        gate.run("echo $SASSE_CANDIDATE", &c, &log).unwrap();
        assert!(std::fs::read_to_string(&log).unwrap().contains(c.as_str()));
    }
}
