# Branch ADR: releases

## Meta
- **Branch**: `feat/releases`
- **Type**: feat
- **Status**: accepted
- **Created**: 2026-09-16
- **Author**: thoroc
- **PR**: See the repository history; this landed through a pull request like
  everything else.

## Problem Statement
### Context
sasse had no releases at all: no tags, no GitHub releases, `version = "0.1.0"`
never bumped since the bootstrap commit. The only way to obtain it was to clone
and `cargo build`.

That is a defensible position for a personal tool, but it sits awkwardly with
two things this project already does. It pins every tool it uses to an exact
version through `mise.lock`, while having no version of its own that anyone
could pin. And its queue database carries migrations, so "which sasse wrote this
queue" is a question that can already be asked and currently has no answer.

### Goals
- A version that means something, so `sasse --version` and a migration history
  can be reconciled.
- An install path that does not require a Rust toolchain.
- A changelog that follows from the commits rather than from memory.
- No new long-lived credentials.

### Non-Goals
- Publishing to crates.io. Considered and rejected below.
- Windows binaries. sasse is effectively Unix-only, which is recorded under
  Alternatives so the omission reads as a decision.
- A release cadence. This says how a release happens, not when.

## Decision Record
### Options Considered

Three separate choices, each with real alternatives.

**What a release is.**

*Tagged GitHub releases with prebuilt binaries.* Chosen. A tag, a changelog
entry, and archives attached to the release, so it installs without cargo.

*Publish to crates.io.* The most useful distribution for a Rust CLI and the
least infrastructure, since cargo does the building. Rejected: it commits the
project to semver for strangers and to a name in a shared namespace, for a tool
that currently has one user. It can be added later without undoing any of this,
which is the property that made it safe to decline.

*Tags only, with no artefacts.* Cheapest thing that is not nothing, and it would
have satisfied the version-meaning goal. Rejected because the install path was
the other half of the point.

*No releases at all.* Rejected, since the two awkwardnesses above are real and
neither goes away on its own.

**How the version and changelog are produced.**

*release-plz in git-only mode.* Chosen. It computes the semver bump from
conventional commits, which this repository already writes, and generates the
changelog with git-cliff underneath.

*release-please.* The same shape, language-agnostic, and with no interest in
crates.io so there would be nothing to disable. Rejected as a second tool with
no other use here, configured rather than inferring the Cargo version.

*git-cliff plus a hand-cut tag.* Fewest moving parts, and git-cliff is already
what release-plz uses internally. Rejected because the bump and the tag become
manual steps, which are the steps that get skipped.

*Fully manual.* Rejected: a changelog maintained by hand stops matching the
commits exactly when someone is in a hurry, which is when it earns its keep.

**Which token drives it.**

*`GITHUB_TOKEN` alone.* Chosen, and it required changing the pipeline's shape
rather than its credentials. See the rationale.

*A fine-grained personal access token.* Needed only if a bot should open the
release pull request automatically. Rejected: a long-lived credential in the
repository's secrets, bought for an automation that replaces one local command.

*A GitHub App.* What the release-plz project uses for itself: no long-lived
credential, and the release PR is authored by a bot. Rejected as considerably
more setup, an app, a private key and two secrets, for the same automation.

**Which platforms get binaries.**

*macOS arm64 and Linux x86_64.* Chosen: the two platforms this project actually
runs on, and both are runners the CI already uses.

*macOS arm64 only.* Rejected because Linux is where a resident worker would
sensibly live.

*Four targets, adding macOS x86_64 and Linux aarch64.* Rejected as builds for
hardware nobody involved has, which break unnoticed.

*Adding Windows.* Rejected on a finding rather than a preference. sasse is
effectively Unix-only: the gate and the `on_settle` hook both run `/bin/sh`, the
hook timeout kills a process group, and clean shutdown uses POSIX signals. The
`#[cfg(not(unix))]` stubs make it compile, not work, so a Windows binary would
be one that builds and then does nothing correctly.

### Chosen Solution
release-plz in git-only mode computes the bump and the changelog. The bump lands
as an ordinary pull request. Merging it triggers one workflow that tags, builds
both binaries and attaches them, using `GITHUB_TOKEN` and nothing else.

Three specifics carry the decision:

**`git_only = true`, not `publish = false`.** The obvious reading of "do not
publish to crates.io" is `publish = false`, and that is wrong here. Its
documentation says plainly that with it disabled, "release-plz will still use the
cargo registry to check what's the latest release". For a crate that has never
been published, that check has nothing to find, every time. `git_only` instead
determines versions from git tags, is documented as being "useful for packages
that are not published to crates.io but still need version management", and skips
`cargo publish` as a consequence rather than as a separate setting. Setting
`publish = true` alongside it is an error, so it replaces that field rather than
accompanying it.

**The release pull request is opened by a person, not by release-plz.** This is
what lets `GITHUB_TOKEN` suffice, and it is the whole of the token decision.

**One workflow does everything after the merge.** It tags, then builds both
targets in a matrix, then uploads the archives, all within a single run. Nothing
in the pipeline waits on a workflow started by another workflow.

### Rationale
The token question looked like a credentials problem and was a topology problem.

`GITHUB_TOKEN` has exactly one relevant limitation: it cannot trigger further
workflow runs. Everything else it needs here is grantable per job. Two
consequences follow, and they are the same consequence twice: a pull request
opened with it never runs the `ci` check, and a tag pushed with it never starts a
build.

Both mattered because of a rule this repository adopted earlier: every change
lands through a pull request, and `ci` is a required check. A release PR opened by
`GITHUB_TOKEN` would therefore be permanently unmergeable. It would have no `ci`
run, no `ci` run could ever be produced for it, and branch protection would
refuse it forever. Reaching for a PAT at that point treats the symptom.

Removing the automation removes the problem. A release PR opened the same way
every other PR here is opened gets a normal `ci` run for the ordinary reason that
a person opened it. And a pipeline that does its tagging and its building in one
run never needs to start a second one. What remains is a workflow that pushes a
tag, creates a release and uploads two files, all of which `GITHUB_TOKEN` can do
with `contents: write` on a single job.

The automation given up is small and was worth giving up: a release PR that
appears without anyone asking, in exchange for two secrets to maintain and a
credential that outlives the reason it was created.

## Implementation
### Key Changes
- `release-plz.toml`: `git_only = true`, and nothing else, since the defaults are
  right once version detection comes from tags.
- `.github/workflows/release.yml`: one workflow on push to `main`. A first job
  runs `release-plz release` and reports whether it tagged anything; a matrix job
  then builds and uploads. Concurrency is grouped and never cancelled, so two
  pushes cannot race a release.
- `mise.toml`: `release-plz` pinned like every other tool, and a
  `release:prepare` task so the local step is discoverable rather than folklore.
- `README.md` and `CONTRIBUTING.md`: how to cut one.

### Testing Strategy
- The workflow's YAML parses and every action is pinned to a full commit SHA,
  asserted the same way `ci.yml` is.
- `release-plz update` runs locally and produces a bump and a changelog from the
  existing history, confirming git-only mode finds no tag and treats the result
  as an initial release.
- Every shell step is shellcheck-clean, extracted from the YAML and checked the
  way `ci.yml`'s steps were.
- The first real release is itself the end-to-end test, and its absence of
  prebuilt binaries would be the visible failure.

## Challenges & Solutions
Two wrong turns, both recorded because they are easy to repeat.

The first was reading "do not publish to crates.io" as `publish = false`. That
setting leaves version detection pointed at a registry the crate is not in.
`git_only` is the field that exists for this, and finding it required reading the
configuration reference rather than guessing from the field names.

The second was proposing a repository settings change to let Actions open pull
requests, having concluded that Actions could not. Actions can; the default
`GITHUB_TOKEN` is what is constrained, and the constraint is about triggering
workflows rather than about creating pull requests. Flipping that setting would
have produced a release pull request that no `ci` run could ever attach to, and
therefore one that branch protection would refuse forever. The fix was to stop
wanting a bot to open it.

## Impact Assessment
- **Performance**: one extra workflow per push to `main`, which no-ops unless the
  version in `Cargo.toml` has no tag. Two build jobs on the pushes that do
  release.
- **Security**: no new credentials. `GITHUB_TOKEN` with `contents: write` on the
  jobs that need it; `default_workflow_permissions` stays read-only and the
  create-pull-request setting stays off. The release-plz action is pinned to a
  full commit SHA like every other action here, which matters more for a job
  holding write access than for one holding read.
- **Maintenance**: a release now has a manual first step, running
  `mise run release:prepare` and opening the resulting PR. That is deliberate,
  and the reason is above, but it is a step that can be forgotten in a way an
  automatic release PR could not be.

## Risks & Pitfalls
- **Risk**: nobody remembers to run the prepare step, so the changelog and the
  version drift behind `main`.
  **Mitigation**: accepted, and it is the cost of the token decision. It is
  visible, because `sasse --version` stops matching what is on `main`.
- **Risk**: `release_always` defaults to releasing on every push, so an
  accidental version bump in an unrelated pull request would cut a release.
  **Mitigation**: the bump only happens in a file that nothing else edits, and
  every change is reviewed in a pull request before it can reach `main`.
- **Risk**: the release job runs with `contents: write` while `ci` runs with
  read-only, so it is the more attractive of the two to compromise.
  **Mitigation**: the action is SHA-pinned, permissions are per-job rather than
  workflow-wide, and `zizmor` already lints these files on any pull request that
  touches them.
- **Risk**: a half-finished release, where the tag and GitHub release exist but
  an upload failed, leaving a release with missing binaries.
  **Mitigation**: `fail-fast` is off so one target failing does not cancel the
  other, and re-running the matrix job uploads with `--clobber`. Accepted that
  the window exists.

## Outcome & Lessons
Pending. To be filled in after the first real release, which will show whether
git-only mode behaves as documented on a repository with no tags at all, and
whether the manual prepare step is an acceptable price or an irritation.
