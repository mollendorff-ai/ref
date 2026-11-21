# ADR-004: Strict Quality Rules

**Status:** Accepted
**Date:** 2026-11-16
**Supersedes:** [ADR-003](ADR-003-TEST-STRATEGY.md)
**Author:** Claude (AI assistant, under the maintainer's direction)

---

## Context

ADR-003 split tests into unit tests and Chrome tests, marked the Chrome tests `#[ignore]`, and left 12 files out of the 100% coverage gate.
The gate was green, and the README badge said 100%, but measured over all code with every test run, coverage was 78.03% on 2026-11-16 (5,125 lines, 1,126 not covered).
A gate that leaves out the code it finds hard is not a 100% gate.

## Decision

Four rules, set by the maintainer as non-negotiable:

1. **100% line coverage of all code, nothing skipped.**
   No `--ignore-filename-regex`, no `#[ignore]`, no `#[coverage(off)]`.
   Code that is hard to test is changed so it can be tested; it is never excluded.
2. **Pedantic linting and formatting, nothing skipped.**
   `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings -W clippy::pedantic`.
   No new `#[allow(clippy::...)]`.
3. **All tests inline**, in `#[cfg(test)] mod tests` inside the file they test.
   No `tests/*.rs` files; test data may stay in `tests/fixtures/`.
4. **No source file over 1,500 lines**, tests included.
   A file that would pass the limit is split into a module folder along its real seams.

Tests that need Chrome run in plain `cargo test`, with no `#[ignore]`.
Chrome is a requirement of the tool, so it is a requirement of its tests.

## How the rules are met

- **Command-line tests**: `main` becomes a thin wrapper over a library function `cli::run(args) -> ExitCode`, which inline tests call in-process instead of starting the binary.
  A thin layer of process-level tests on top of this is proposed in [ADR-006](ADR-006-PROCESS-LEVEL-TESTS.md).
- **Chrome tests**: move into the modules they exercise (`fetch`, `browser`, `pdf`), using the existing fixtures and a local HTTP server.
- **Network code** (`update`, `check_links`, `verify_refs`, `refresh_data`): the base URL is a parameter, so tests point it at a local server (`wiremock` is already a dev dependency).
- **File code** (`init`, `scan`): tests use temporary folders.
- **MCP server**: tools are called in-process.
- **`login`**: opens a visible Chrome; it gets a seam so tests can run it headless.
- **CI and Makefile**: the exclusion regex goes, the gate becomes `cargo llvm-cov --fail-under-lines 100` over everything, and the README's "Logic coverage" badge becomes a plain coverage badge.

## Enforcement

The rules are written in three places so every agent sees them: `CLAUDE.md` (loaded by Claude Code in every session), `.asimov/project.yaml` (read by Asimov for any AI CLI) and this ADR.
Only CI enforces them: it fails on any breach, and branch protection on `main` requiring the CI checks keeps a failing change from merging.

## Consequences

### Positive

- The coverage number means what it says.
- Network and file code gets tested, not just assumed.
- One place to look for a module's tests.

### Negative

- `cargo test` needs Chrome and takes about 15 seconds instead of under one.
- Seams for testing (injected URLs and folders) add a little code.
- Getting from 78% to 100% is a series of pull requests, tracked as `STRICT_RULES` in `.asimov/roadmap.yaml`.
