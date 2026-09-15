//! Running the gate against an assembled candidate.
//!
//! Behind a trait for the same reason as the git operations: so the worker's
//! decisions can be tested without waiting on a real test suite, and so a
//! failing gate can be arranged exactly rather than approximated.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use eyre::{Result, WrapErr};

use crate::bytes::ByteSize;
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
    /// Run `command` against the candidate currently checked out, writing at
    /// most `max_log_size` bytes of its combined output to `log_path`.
    ///
    /// The candidate id is passed so the run can be identified in logs. The
    /// gate is not expected to use it to find the code: the code is whatever is
    /// checked out.
    fn run(
        &self,
        command: &str,
        candidate: &Sha,
        log_path: &Path,
        max_log_size: ByteSize,
    ) -> Result<Verdict>;
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
    fn run(
        &self,
        command: &str,
        candidate: &Sha,
        log_path: &Path,
        max_log_size: ByteSize,
    ) -> Result<Verdict> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating the log directory {}", parent.display()))?;
        }

        let log = File::create(log_path)
            .wrap_err_with(|| format!("creating the gate log {}", log_path.display()))?;

        // `exec 2>&1` merges the gate's stderr into its stdout inside the shell,
        // so the parent reads one stream and the cap applies to the output as a
        // whole rather than to each half separately.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("exec 2>&1\n{command}"))
            .current_dir(&self.integration)
            .env("SASSE_CANDIDATE", candidate.as_str())
            // stdin closed: a gate that waits for input would hang the queue
            // with no indication of why.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .wrap_err_with(|| format!("running the gate: {command}"))?;

        let mut output = child.stdout.take().expect("stdout was piped");
        let mut capped = Capped::new(log, max_log_size);
        let mut buffer = [0u8; 16 * 1024];

        loop {
            let read = output
                .read(&mut buffer)
                .wrap_err("reading the gate's output")?;
            if read == 0 {
                break;
            }
            capped
                .take(&buffer[..read])
                .wrap_err_with(|| format!("writing the gate log {}", log_path.display()))?;
        }

        capped
            .finish()
            .wrap_err_with(|| format!("finishing the gate log {}", log_path.display()))?;

        let status = child.wait().wrap_err("waiting for the gate")?;
        if status.success() {
            Ok(Verdict::Passed)
        } else {
            Ok(Verdict::Failed {
                exit_code: status.code(),
            })
        }
    }
}

/// Writes a stream to a file, keeping its beginning and its end.
///
/// The head carries the gate command's own startup output, where a
/// misconfigured gate announces itself; the tail carries the failure. The
/// middle of a test run is almost always the cases that passed. Written as a
/// single pass so an enormous log never lands on disk in full, which is the
/// point: truncating afterwards would mean having stored it first.
struct Capped {
    file: File,
    head_room: usize,
    tail: VecDeque<u8>,
    tail_capacity: usize,
    dropped: u64,
}

impl Capped {
    fn new(file: File, cap: ByteSize) -> Self {
        // Split the allowance between the two ends. At least one byte each, so
        // a nonsensically small cap still behaves.
        let half = (cap.bytes() / 2).max(1) as usize;
        Self {
            file,
            head_room: half,
            tail: VecDeque::with_capacity(half.min(64 * 1024)),
            tail_capacity: half,
            dropped: 0,
        }
    }

    fn take(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        let mut rest = chunk;

        if self.head_room > 0 {
            let take = self.head_room.min(rest.len());
            self.file.write_all(&rest[..take])?;
            self.head_room -= take;
            rest = &rest[take..];
        }

        for byte in rest {
            if self.tail.len() == self.tail_capacity {
                self.tail.pop_front();
                self.dropped += 1;
            }
            self.tail.push_back(*byte);
        }

        Ok(())
    }

    fn finish(mut self) -> std::io::Result<()> {
        if self.dropped > 0 {
            write!(
                self.file,
                "\n[sasse dropped {} bytes from the middle of this log]\n",
                self.dropped
            )?;
        }
        let tail: Vec<u8> = self.tail.into_iter().collect();
        self.file.write_all(&tail)?;
        self.file.flush()
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
        fn run(
            &self,
            command: &str,
            candidate: &Sha,
            _log_path: &Path,
            _max_log_size: ByteSize,
        ) -> Result<Verdict> {
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

    const GENEROUS: ByteSize = ByteSize::new(1024 * 1024);

    fn candidate() -> Sha {
        Sha::parse(&"ab".repeat(20)).unwrap()
    }

    #[test]
    fn a_succeeding_command_passes() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("logs/run.log");

        assert_eq!(
            gate.run("true", &candidate(), &log, GENEROUS).unwrap(),
            Verdict::Passed
        );
    }

    #[test]
    fn a_failing_command_reports_its_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());

        assert_eq!(
            gate.run(
                "exit 3",
                &candidate(),
                &dir.path().join("run.log"),
                GENEROUS
            )
            .unwrap(),
            Verdict::Failed { exit_code: Some(3) }
        );
    }

    #[test]
    fn output_from_both_streams_is_captured() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");

        gate.run("echo out; echo err 1>&2", &candidate(), &log, GENEROUS)
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

        gate.run("true", &candidate(), &log, GENEROUS).unwrap();
        assert!(log.exists());
    }

    #[test]
    fn the_gate_runs_in_the_integration_checkout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "here").unwrap();
        let gate = ShellGate::new(dir.path());

        assert_eq!(
            gate.run(
                "test -f marker",
                &candidate(),
                &dir.path().join("run.log"),
                GENEROUS
            )
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

        assert_eq!(
            gate.run("read line", &candidate(), &log, GENEROUS).unwrap(),
            Verdict::Failed { exit_code: Some(1) }
        );
    }

    #[test]
    fn the_candidate_is_available_to_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");
        let c = candidate();

        gate.run("echo $SASSE_CANDIDATE", &c, &log, GENEROUS)
            .unwrap();
        assert!(std::fs::read_to_string(&log).unwrap().contains(c.as_str()));
    }

    /// Under the cap, the log must be exactly what the gate wrote.
    #[test]
    fn a_log_within_the_cap_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");

        gate.run("printf 'hello\\n'", &candidate(), &log, GENEROUS)
            .unwrap();

        assert_eq!(std::fs::read_to_string(&log).unwrap(), "hello\n");
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("sasse dropped"),
            "nothing was dropped, so nothing should say so"
        );
    }

    #[test]
    fn a_log_over_the_cap_keeps_its_head_and_its_tail() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");

        // 20,000 lines of six bytes each, far past a 4KB cap.
        gate.run(
            "for i in $(seq 1 20000); do printf 'L%05d\\n' \"$i\"; done",
            &candidate(),
            &log,
            ByteSize::new(4096),
        )
        .unwrap();

        let kept = std::fs::read_to_string(&log).unwrap();
        assert!(kept.contains("L00001"), "the head is missing");
        assert!(kept.contains("L20000"), "the tail is missing");
        assert!(
            !kept.contains("L10000"),
            "the middle should have gone, not been kept"
        );
        assert!(
            kept.contains("sasse dropped"),
            "a truncated log must say so: {}",
            &kept[..kept.len().min(200)]
        );
    }

    /// The cap is the point, so it has to actually hold.
    #[test]
    fn a_capped_log_stays_near_the_cap_however_much_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");
        let cap = ByteSize::new(4096);

        gate.run(
            "for i in $(seq 1 50000); do printf 'L%05d\\n' \"$i\"; done",
            &candidate(),
            &log,
            cap,
        )
        .unwrap();

        let size = std::fs::metadata(&log).unwrap().len();
        let marker_allowance = 128;
        assert!(
            size <= cap.bytes() + marker_allowance,
            "log grew to {size} bytes against a cap of {cap}"
        );
    }

    /// A gate whose whole output is one enormous line must still be bounded,
    /// which is why the cap is in bytes rather than lines.
    #[test]
    fn a_single_enormous_line_is_still_capped() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");
        let cap = ByteSize::new(2048);

        gate.run(
            "for i in $(seq 1 20000); do printf 'xxxxxxxxxx'; done",
            &candidate(),
            &log,
            cap,
        )
        .unwrap();

        let size = std::fs::metadata(&log).unwrap().len();
        assert!(size <= cap.bytes() + 128, "one line grew to {size} bytes");
    }

    #[test]
    fn a_verdict_is_unaffected_by_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let gate = ShellGate::new(dir.path());
        let log = dir.path().join("run.log");

        let verdict = gate
            .run(
                "for i in $(seq 1 5000); do echo noise; done; exit 7",
                &candidate(),
                &log,
                ByteSize::new(1024),
            )
            .unwrap();

        assert_eq!(verdict, Verdict::Failed { exit_code: Some(7) });
    }
}
