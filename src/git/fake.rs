//! An in-memory [`Git`] for testing the worker without a repository.
//!
//! Real git is exercised separately, in `command`'s own tests against actual
//! repositories. This exists so the worker loop's decisions can be tested
//! directly, including outcomes that are tedious to arrange for real, such as
//! a base branch moving at the exact moment a candidate is ready to land.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use eyre::{Result, eyre};

use super::{Git, MergeOutcome, RefUpdate, Sha, Worktree};

pub struct FakeGit {
    state: Mutex<State>,
}

struct State {
    refs: HashMap<String, Sha>,
    worktrees: Vec<Worktree>,
    head: Option<Sha>,
    /// Commits that conflict when merged.
    conflicting: HashSet<Sha>,
    /// Counter behind the synthesised candidate ids, so a test can predict
    /// them.
    minted: u32,
}

impl FakeGit {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                refs: HashMap::new(),
                worktrees: Vec::new(),
                head: None,
                conflicting: HashSet::new(),
                minted: 0,
            }),
        }
    }

    /// A deterministic object id, so assertions can name one.
    pub fn commit(seed: u8) -> Sha {
        Sha::parse(&format!("{seed:02x}").repeat(20)).expect("20 bytes is a valid object id")
    }

    pub fn with_branch(self, refname: &str, at: &Sha) -> Self {
        self.state
            .lock()
            .unwrap()
            .refs
            .insert(normalise(refname), at.clone());
        self
    }

    pub fn with_checkout(self, path: &str, branch: Option<&str>) -> Self {
        self.state.lock().unwrap().worktrees.push(Worktree {
            path: PathBuf::from(path),
            branch: branch.map(|b| b.to_string()),
        });
        self
    }

    /// Make merging this commit conflict.
    pub fn with_conflict(self, at: &Sha) -> Self {
        self.state.lock().unwrap().conflicting.insert(at.clone());
        self
    }

    /// Move a branch behind the worker's back, to stage a stale base.
    pub fn force_branch(&self, refname: &str, to: &Sha) {
        self.state
            .lock()
            .unwrap()
            .refs
            .insert(normalise(refname), to.clone());
    }

    pub fn head(&self) -> Option<Sha> {
        self.state.lock().unwrap().head.clone()
    }
}

impl Default for FakeGit {
    fn default() -> Self {
        Self::new()
    }
}

impl Git for FakeGit {
    fn resolve(&self, refname: &str) -> Result<Sha> {
        self.state
            .lock()
            .unwrap()
            .refs
            .get(&normalise(refname))
            .cloned()
            .ok_or_else(|| eyre!("no such ref: {refname}"))
    }

    fn worktrees(&self) -> Result<Vec<Worktree>> {
        Ok(self.state.lock().unwrap().worktrees.clone())
    }

    fn checkout_detached(&self, at: &Sha) -> Result<()> {
        self.state.lock().unwrap().head = Some(at.clone());
        Ok(())
    }

    fn merge(&self, commit: &Sha, _message: &str) -> Result<MergeOutcome> {
        let mut state = self.state.lock().unwrap();
        if state.head.is_none() {
            return Err(eyre!("nothing checked out to merge into"));
        }
        if state.conflicting.contains(commit) {
            return Ok(MergeOutcome::Conflicted);
        }
        state.minted += 1;
        let merged = Sha::parse(&format!("{:040x}", 0xc0ffee00u64 + u64::from(state.minted)))
            .expect("a synthesised id is 40 hex digits");
        state.head = Some(merged.clone());
        Ok(MergeOutcome::Merged(merged))
    }

    fn fast_forward(&self, refname: &str, from: &Sha, to: &Sha) -> Result<RefUpdate> {
        let mut state = self.state.lock().unwrap();
        let key = normalise(refname);
        match state.refs.get(&key) {
            None => Err(eyre!("no such ref: {refname}")),
            Some(current) if current != from => Ok(RefUpdate::Stale),
            Some(_) => {
                state.refs.insert(key, to.clone());
                Ok(RefUpdate::Updated)
            }
        }
    }
}

fn normalise(refname: &str) -> String {
    refname
        .strip_prefix("refs/heads/")
        .unwrap_or(refname)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::checkouts_holding;

    #[test]
    fn a_branch_resolves_however_it_is_spelled() {
        let base = FakeGit::commit(1);
        let git = FakeGit::new().with_branch("refs/heads/main", &base);
        assert_eq!(git.resolve("main").unwrap(), base);
        assert_eq!(git.resolve("refs/heads/main").unwrap(), base);
    }

    #[test]
    fn merging_advances_head_to_a_new_commit() {
        let base = FakeGit::commit(1);
        let git = FakeGit::new();
        git.checkout_detached(&base).unwrap();

        let MergeOutcome::Merged(first) = git.merge(&FakeGit::commit(2), "c1").unwrap() else {
            panic!("expected a merge");
        };
        let MergeOutcome::Merged(second) = git.merge(&FakeGit::commit(3), "c2").unwrap() else {
            panic!("expected a merge");
        };
        assert_ne!(first, second, "each merge is a distinct commit");
        assert_eq!(git.head(), Some(second));
    }

    #[test]
    fn a_commit_marked_conflicting_conflicts() {
        let bad = FakeGit::commit(9);
        let git = FakeGit::new().with_conflict(&bad);
        git.checkout_detached(&FakeGit::commit(1)).unwrap();
        assert_eq!(git.merge(&bad, "c1").unwrap(), MergeOutcome::Conflicted);
    }

    #[test]
    fn a_moved_branch_reports_stale() {
        let base = FakeGit::commit(1);
        let git = FakeGit::new().with_branch("main", &base);
        git.force_branch("main", &FakeGit::commit(2));
        assert_eq!(
            git.fast_forward("main", &base, &FakeGit::commit(3))
                .unwrap(),
            RefUpdate::Stale
        );
    }

    #[test]
    fn the_fake_agrees_with_the_guard_about_checkouts() {
        let git = FakeGit::new()
            .with_checkout("/primary", Some("refs/heads/main"))
            .with_checkout("/integration", None);
        assert_eq!(
            checkouts_holding(&git, "main").unwrap(),
            vec![PathBuf::from("/primary")]
        );
    }
}
