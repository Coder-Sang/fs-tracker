# Changelog

All notable changes to fs-tracker are documented here.

## [Unreleased]

## [0.2.0] - 2026-09-18

### Added

- Add a versioned JSON tracking policy for roots, exclusions, and capture
  limits, accepted by `fs-tracker run --config` and
  `fs-tracker-git-receipt --config`, while retaining the existing repeated
  `--root`, `--exclude`, and `--project` arguments.
- Add recursive component exclusions through policy `recursiveExclusions` and
  CLI `--exclude-recursive`; rules apply at any depth without prefix matching
  names such as `.github` for `.git`.

### Changed

- Index tracked roots by normalized path and exact exclusions by root to avoid
  linear root scans for every captured filesystem event.
- Report recursive exclusions in coverage and advance `policyVersion` to 2.

## [0.1.1] - 2026-09-18

### Fixed

- Create capture output directories with mode `0700` in the directory creation
  operation, avoiding a follow-up permission mutation that is unsupported by
  some filesystems.

### Added

- Add a regression test covering the private capture output mode.
