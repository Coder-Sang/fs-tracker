# Changelog

All notable changes to fs-tracker are documented here.

## [0.1.1] - 2026-09-18

### Fixed

- Create capture output directories with mode `0700` in the directory creation
  operation, avoiding a follow-up permission mutation that is unsupported by
  some filesystems.

### Added

- Add a regression test covering the private capture output mode.
