//! The real implementation, shelling out to `git`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use eyre::{Result, WrapErr, eyre};

use super::{Git, MergeOutcome, RefUpdate, Sha, Worktree};

/// The variables through which an ambient environment names a repository.
///
/// `-C` sets the directory git starts from, and every one of these outranks
/// it: with `GIT_DIR` set, git never looks at the directory at all. So every
/// invocation here would operate on whichever repository the environment
/// happened to name, not the one it was handed.
///
/// That environment is not exotic. git exports these to its own hooks, and
/// sasse is run from a pre-push hook, from a gate command git started, and
/// from an `on_settle` hook, so the queue can find itself pointed at a
/// repository nobody asked about.
const AMBIENT_REPOSITORY: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// A git invocation in `dir`, deaf to the environment's idea of which
/// repository it is in.
///
/// The only way to reach git from this module, so production and the tests
/// cannot disagree about what has been cleared.
fn git_in(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    for key in AMBIENT_REPOSITORY {
        command.env_remove(key);
    }
    command
}

pub struct CommandGit {
    /// The repository whose refs are read and moved.
    repo: PathBuf,
    /// The checkout candidates are built and gated in. It runs on a detached
    /// HEAD, so nothing done here touches a branch anyone has checked out.
    integration: PathBuf,
}

impl CommandGit {
    pub fn new(repo: impl Into<PathBuf>, integration: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            integration: integration.into(),
        }
    }

    pub fn integration_path(&self) -> &Path {
        &self.integration
    }

    /// Run git and hand back the outcome, whatever it was.
    fn run<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<Output> {
        git_in(dir)
            .args(args)
            .output()
            .wrap_err_with(|| format!("running git in {}", dir.display()))
    }

    /// Run git, insisting it succeeded, and return trimmed stdout.
    fn checked<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<String> {
        let out = self.run(dir, args)?;
        if !out.status.success() {
            return Err(eyre!(
                "git {} failed in {}: {}",
                describe(args),
                dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn resolve_in(&self, dir: &Path, rev: &str) -> Result<Sha> {
        let raw = self.checked(
            dir,
            &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
        )?;
        Sha::parse(&raw)
    }

    fn merge_in_progress(&self) -> bool {
        self.run(
            &self.integration,
            &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"],
        )
        .map(|o| o.status.success())
        .unwrap_or(false)
    }
}

impl Git for CommandGit {
    fn resolve(&self, refname: &str) -> Result<Sha> {
        self.resolve_in(&self.repo, refname)
    }

    fn worktrees(&self) -> Result<Vec<Worktree>> {
        let out = self.checked(&self.repo, &["worktree", "list", "--porcelain"])?;
        Ok(parse_worktrees(&out))
    }

    fn checkout_detached(&self, at: &Sha) -> Result<()> {
        self.checked(
            &self.integration,
            &["checkout", "--force", "--detach", at.as_str()],
        )?;
        Ok(())
    }

    fn merge(&self, commit: &Sha, message: &str) -> Result<MergeOutcome> {
        // --no-ff so the candidate is the merge result even when the branch is
        // a descendant, which keeps what was gated identical to what lands.
        let out = self.run(
            &self.integration,
            &[
                "merge",
                "--no-ff",
                "--no-edit",
                "-m",
                message,
                commit.as_str(),
            ],
        )?;

        if out.status.success() {
            return Ok(MergeOutcome::Merged(
                self.resolve_in(&self.integration, "HEAD")?,
            ));
        }

        // Distinguish a conflict from a broken invocation by asking git what
        // state it is in, rather than by matching on its prose.
        if self.merge_in_progress() {
            self.checked(&self.integration, &["merge", "--abort"])
                .wrap_err("abandoning a conflicted merge")?;
            return Ok(MergeOutcome::Conflicted);
        }

        Err(eyre!(
            "merging {commit} failed without leaving a merge in progress: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }

    fn read_file_at(&self, at: &Sha, path: &str) -> Result<Option<String>> {
        let out = self.run(&self.repo, &["show", &format!("{at}:{path}")])?;
        if out.status.success() {
            return Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()));
        }
        // A path absent from the commit is an ordinary answer, not a fault. Any
        // other failure is worth raising.
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("does not exist") || stderr.contains("exists on disk, but not in") {
            return Ok(None);
        }
        Err(eyre!("reading {path} at {at} failed: {}", stderr.trim()))
    }

    fn fast_forward(&self, refname: &str, from: &Sha, to: &Sha) -> Result<RefUpdate> {
        // update-ref takes the ref name literally and will not apply the usual
        // resolution rules, so "main" has to be spelled out. rev-parse, used by
        // resolve, accepts either, which is why a short name reads correctly and
        // then fails to write.
        let qualified = qualify(refname);

        // update-ref with an expected old value is the atomic compare-and-set.
        let out = self.run(
            &self.repo,
            &["update-ref", &qualified, to.as_str(), from.as_str()],
        )?;

        if out.status.success() {
            return Ok(RefUpdate::Updated);
        }

        // It refused. If the branch is no longer where we expected, that is the
        // ordinary stale case; anything else is a real failure worth raising.
        match self.resolve(refname) {
            Ok(current) if current != *from => Ok(RefUpdate::Stale),
            _ => Err(eyre!(
                "moving {refname} from {from} to {to} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        }
    }
}

/// Spell a branch name out as a full ref, leaving an already-qualified name
/// alone.
fn qualify(refname: &str) -> String {
    if refname.starts_with("refs/") {
        refname.to_string()
    } else {
        format!("refs/heads/{refname}")
    }
}

/// Records separated by blank lines, each starting `worktree <path>`, with
/// either `branch <ref>` or `detached`. Verified against git 2.55.
fn parse_worktrees(porcelain: &str) -> Vec<Worktree> {
    let mut found = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    let mut flush = |path: &mut Option<PathBuf>, branch: &mut Option<String>| {
        if let Some(p) = path.take() {
            found.push(Worktree {
                path: p,
                branch: branch.take(),
            });
        }
    };

    for line in porcelain.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch);
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.to_string());
        }
    }
    flush(&mut path, &mut branch);

    found
}

fn describe<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::git::checkouts_holding;

    struct Fixture {
        _dir: TempDir,
        git: CommandGit,
        base: Sha,
        /// A branch that merges cleanly onto base.
        clean: Sha,
        /// A branch that conflicts with `clean`.
        conflicting: Sha,
    }

    fn run(dir: &Path, args: &[&str]) {
        let out = git_in(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn sha(dir: &Path, rev: &str) -> Sha {
        let out = git_in(dir).args(["rev-parse", rev]).output().unwrap();
        Sha::parse(&String::from_utf8_lossy(&out.stdout)).unwrap()
    }

    /// A repository whose base branch is checked out nowhere, which is the
    /// layout the queue requires, plus a detached integration worktree.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        run(&repo, &["init", "-q", "-b", "main"]);
        // Local config, so a developer's global signing or identity settings
        // cannot change the outcome of these tests.
        run(&repo, &["config", "user.email", "queue@example.invalid"]);
        run(&repo, &["config", "user.name", "sasse tests"]);
        run(&repo, &["config", "commit.gpgsign", "false"]);

        std::fs::write(repo.join("shared"), "base\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "base"]);
        let base = sha(&repo, "HEAD");

        run(&repo, &["switch", "-q", "-c", "clean"]);
        std::fs::write(repo.join("only-clean"), "clean\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "clean"]);
        let clean = sha(&repo, "HEAD");

        run(&repo, &["switch", "-q", "--detach", base.as_str()]);
        run(&repo, &["switch", "-q", "-c", "conflicting"]);
        std::fs::write(repo.join("only-clean"), "conflicting\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "conflicting"]);
        let conflicting = sha(&repo, "HEAD");

        // Leave the primary checkout detached so no branch is held by it.
        run(&repo, &["switch", "-q", "--detach", base.as_str()]);

        let integration = dir.path().join("integration");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                integration.to_str().unwrap(),
                base.as_str(),
            ],
        );

        let git = CommandGit::new(&repo, &integration);
        Fixture {
            _dir: dir,
            git,
            base,
            clean,
            conflicting,
        }
    }

    /// Every variable through which the environment could name a repository is
    /// cleared, so `-C` decides where the invocation lands and nothing else
    /// does. Asserted on the command rather than by setting these for real,
    /// because the process environment is shared by every test running beside
    /// this one.
    #[test]
    fn git_runs_deaf_to_an_ambient_repository() {
        let command = git_in(Path::new("/nonexistent"));

        let cleared: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();

        for key in AMBIENT_REPOSITORY {
            assert!(
                cleared.contains(&key.to_string()),
                "{key} is still inherited"
            );
        }
    }

    #[test]
    fn a_branch_resolves_to_its_commit() {
        let f = fixture();
        assert_eq!(f.git.resolve("refs/heads/clean").unwrap(), f.clean);
        assert_eq!(f.git.resolve("clean").unwrap(), f.clean);
    }

    #[test]
    fn resolving_a_branch_that_does_not_exist_is_an_error() {
        let f = fixture();
        assert!(f.git.resolve("refs/heads/nope").is_err());
    }

    #[test]
    fn a_clean_branch_merges_into_a_candidate() {
        let f = fixture();
        f.git.checkout_detached(&f.base).unwrap();

        let outcome = f.git.merge(&f.clean, "candidate 1").unwrap();
        let candidate = match outcome {
            MergeOutcome::Merged(sha) => sha,
            MergeOutcome::Conflicted => panic!("a clean branch must not conflict"),
        };
        assert_ne!(candidate, f.base, "the candidate is a new commit");
        assert_eq!(
            f.git.resolve_in(f.git.integration_path(), "HEAD").unwrap(),
            candidate
        );
    }

    #[test]
    fn a_conflict_is_a_verdict_and_leaves_no_merge_in_progress() {
        let f = fixture();
        f.git.checkout_detached(&f.base).unwrap();
        f.git.merge(&f.clean, "candidate 1").unwrap();

        assert_eq!(
            f.git.merge(&f.conflicting, "candidate 2").unwrap(),
            MergeOutcome::Conflicted
        );
        assert!(
            !f.git.merge_in_progress(),
            "a conflicted merge must be abandoned, or the next candidate starts dirty"
        );
    }

    #[test]
    fn a_base_branch_moves_when_it_is_where_we_left_it() {
        let f = fixture();
        assert_eq!(
            f.git
                .fast_forward("refs/heads/main", &f.base, &f.clean)
                .unwrap(),
            RefUpdate::Updated
        );
        assert_eq!(f.git.resolve("refs/heads/main").unwrap(), f.clean);
    }

    /// Invariant 2. What was gated is only merged if the base has not moved
    /// underneath the gate.
    #[test]
    fn a_base_branch_that_moved_is_reported_stale_and_left_alone() {
        let f = fixture();
        f.git
            .fast_forward("refs/heads/main", &f.base, &f.conflicting)
            .unwrap();

        assert_eq!(
            f.git
                .fast_forward("refs/heads/main", &f.base, &f.clean)
                .unwrap(),
            RefUpdate::Stale
        );
        assert_eq!(
            f.git.resolve("refs/heads/main").unwrap(),
            f.conflicting,
            "a stale update must change nothing"
        );
    }

    /// `git update-ref` does not resolve short names, so this asserts against
    /// real git. The in-memory fake normalises ref spellings and is therefore
    /// structurally unable to catch it.
    #[test]
    fn a_short_branch_name_can_still_be_moved() {
        let f = fixture();
        assert_eq!(
            f.git.fast_forward("main", &f.base, &f.clean).unwrap(),
            RefUpdate::Updated
        );
        assert_eq!(f.git.resolve("main").unwrap(), f.clean);
    }

    #[test]
    fn a_short_and_a_fully_qualified_name_move_the_same_branch() {
        let f = fixture();
        f.git.fast_forward("main", &f.base, &f.clean).unwrap();
        assert_eq!(
            f.git.resolve("refs/heads/main").unwrap(),
            f.clean,
            "the short name must address the same ref as the long one"
        );
    }

    #[test]
    fn a_base_branch_checked_out_nowhere_is_safe_to_move() {
        let f = fixture();
        assert!(
            checkouts_holding(&f.git, "refs/heads/main")
                .unwrap()
                .is_empty()
        );
    }

    /// The hazard this guard exists for. Git itself permits the move.
    #[test]
    fn a_checked_out_base_branch_is_reported() {
        let f = fixture();
        let integration = f.git.integration_path().to_path_buf();
        run(&integration, &["switch", "-q", "main"]);

        let holders = checkouts_holding(&f.git, "refs/heads/main").unwrap();
        assert_eq!(holders.len(), 1, "the integration checkout now holds main");
        assert!(holders[0].ends_with("integration"));
    }

    #[test]
    fn a_short_ref_and_a_full_ref_name_the_same_branch() {
        let f = fixture();
        let integration = f.git.integration_path().to_path_buf();
        run(&integration, &["switch", "-q", "main"]);

        assert_eq!(
            checkouts_holding(&f.git, "main").unwrap(),
            checkouts_holding(&f.git, "refs/heads/main").unwrap()
        );
    }

    #[test]
    fn worktrees_are_parsed_with_their_branches() {
        let f = fixture();
        let trees = f.git.worktrees().unwrap();
        assert_eq!(trees.len(), 2, "primary plus integration");
        assert!(
            trees.iter().all(|w| w.branch.is_none()),
            "both are detached in this layout"
        );
    }

    #[test]
    fn a_detached_record_yields_no_branch() {
        let parsed = parse_worktrees(
            "worktree /a\nHEAD aaaa\nbranch refs/heads/main\n\nworktree /b\nHEAD bbbb\ndetached\n",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(parsed[1].branch, None);
    }
}
