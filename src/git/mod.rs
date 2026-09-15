//! The git operations the queue needs, behind a trait.
//!
//! The trait exists so the worker loop can be tested without a repository.
//! [`command::CommandGit`] is the real implementation and shells out to `git`;
//! the fake used in tests keeps refs in memory.
//!
//! Deliberately small. Every method maps onto one git invocation, and the two
//! outcomes that are queue verdicts rather than failures, a merge conflict and
//! a base branch that moved, are returned as values instead of errors.

use std::fmt;
use std::path::PathBuf;

use eyre::{Result, eyre};

pub mod command;

pub use command::CommandGit;

#[cfg(test)]
pub mod fake;

/// A full object id.
///
/// Abbreviations are rejected: the queue compares these for equality to decide
/// whether a base branch moved, and an abbreviation would make that comparison
/// depend on how the id was printed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha(String);

impl Sha {
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        let full_length = s.len() == 40 || s.len() == 64;
        if !full_length || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(eyre!("not a full object id: {raw:?}"));
        }
        Ok(Self(s.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The result of merging one branch into the candidate under construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged(Sha),
    /// A textual conflict. This is a verdict on the entry, not an error: the
    /// entry cannot be integrated at this base and the queue must say so.
    Conflicted,
}

/// The result of moving a base branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefUpdate {
    Updated,
    /// The branch was not where we last saw it, so whatever we gated was gated
    /// against a base that no longer applies. Invariant 2.
    Stale,
}

/// One checkout attached to a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// `None` for a detached HEAD.
    pub branch: Option<String>,
}

pub trait Git {
    /// Resolve a ref to the commit it points at.
    fn resolve(&self, refname: &str) -> Result<Sha>;

    /// Every checkout attached to the repository, including the primary one.
    fn worktrees(&self) -> Result<Vec<Worktree>>;

    /// Put the integration checkout on this commit with a detached HEAD.
    ///
    /// Tracked changes are discarded. Untracked files are left alone, so a
    /// build cache survives from one candidate to the next.
    fn checkout_detached(&self, at: &Sha) -> Result<()>;

    /// Merge a commit into the integration checkout's current HEAD.
    fn merge(&self, commit: &Sha, message: &str) -> Result<MergeOutcome>;

    /// Move a branch from one commit to another, atomically, failing if it is
    /// not currently at `from`.
    fn fast_forward(&self, refname: &str, from: &Sha, to: &Sha) -> Result<RefUpdate>;

    /// Read a file as it exists in a commit, or `None` if the commit has no
    /// such path.
    ///
    /// Out of the commit, deliberately, never off a working tree: the gate
    /// command is read this way, and reading it from disk would let whatever a
    /// candidate leaves in the integration checkout decide what the worker
    /// runs.
    fn read_file_at(&self, at: &Sha, path: &str) -> Result<Option<String>>;
}

/// Checkouts that currently have `refname` checked out. Empty means the ref is
/// safe to move.
///
/// This guard is not optional. Verified against git 2.55: `git update-ref`
/// moves a branch that is checked out, in the current worktree or in another
/// one, with no warning and no refusal, and leaves that checkout's index
/// disagreeing with its HEAD so the whole difference shows up as staged
/// changes. Git will not stop the queue from corrupting somebody's working
/// copy, so the queue stops itself.
///
/// The intended layout follows from this: the base branch is checked out
/// nowhere. The integration checkout runs on a detached HEAD, and developers
/// work on feature branches.
pub fn checkouts_holding(git: &dyn Git, refname: &str) -> Result<Vec<PathBuf>> {
    let wanted = normalise_ref(refname);
    Ok(git
        .worktrees()?
        .into_iter()
        .filter(|w| w.branch.as_deref().map(normalise_ref) == Some(wanted.clone()))
        .map(|w| w.path)
        .collect())
}

/// `main` and `refs/heads/main` name the same branch.
fn normalise_ref(refname: &str) -> String {
    refname
        .strip_prefix("refs/heads/")
        .unwrap_or(refname)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "eb60b2f38342b60bf579eff215cf731d2cf6fe25";

    #[test]
    fn a_full_object_id_parses() {
        assert_eq!(Sha::parse(FULL).unwrap().as_str(), FULL);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(Sha::parse(&format!("  {FULL}\n")).unwrap().as_str(), FULL);
    }

    #[test]
    fn case_is_normalised_so_equality_is_reliable() {
        assert_eq!(
            Sha::parse(&FULL.to_ascii_uppercase()).unwrap(),
            Sha::parse(FULL).unwrap()
        );
    }

    #[test]
    fn an_abbreviated_id_is_rejected() {
        assert!(Sha::parse("eb60b2f").is_err());
    }

    #[test]
    fn a_non_hex_id_is_rejected() {
        assert!(Sha::parse(&"z".repeat(40)).is_err());
    }

    #[test]
    fn a_sha256_length_id_parses() {
        assert!(Sha::parse(&"a".repeat(64)).is_ok());
    }
}
