# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/thoroc/sasse/releases/tag/v0.1.0) - 2026-09-16

### Added

- *(release)* cut tagged releases with prebuilt binaries
- *(notify)* run a command when an entry settles
- *(status)* list every base branch with a queue
- *(logs)* bound gate logs with a budget and keep the reasons
- *(queue)* add hand control of the queue and a way to read gate logs
- *(worker)* add the work loop and a status command
- *(worker)* add the tick
- *(git)* add the git operations behind a trait
- *(lease)* add the single-worker lease
- *(queue)* add queue schema and state machines

### Fixed

- *(notify)* make the orphan test deterministic
- *(notify)* kill a timed-out hook's whole process group
- *(ci)* check for uncommitted tool versions, not an unchanged lockfile

### Other

- *(adr)* record how releases are cut
- *(hk)* correct why the tests sit in pre-push
- add the MIT licence this repository already claimed
- add CLAUDE.md for agents working in this repository
- *(readme)* correct claims that no longer hold
- require pull requests for everything
- *(adr)* record the on-settle hook
- *(adr)* record how several base branches are handled
- run the same checks locally and in CI
- *(adr)* record how gate logs are bounded
- commit the mise lockfile
- *(adr)* record where the gate command is read from
- record what sasse takes from task-spooler
- bootstrap the sasse crate
