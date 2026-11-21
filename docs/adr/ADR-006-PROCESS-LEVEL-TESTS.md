# ADR-006: Process-Level Tests

**Status:** Proposed (2026-11-21)
**Date:** 2026-11-21
**Refines:** [ADR-004](ADR-004-STRICT-QUALITY-RULES.md) (its four rules are unchanged)
**Author:** Claude (AI assistant, under the maintainer's direction)

---

## Context

On 2026-11-21 the final review of the ADR-005 work found that `ref mcp` never read a request.
`cli::main` passed `stdin().lock()` as the command's input and held the lock for the whole run, while the MCP server reads its requests through tokio, which takes the same lock on another thread.
Every MCP client would have timed out.
The bug had come in with moving `main` into `cli` and was never released.

The gate was green the whole time: 100% line coverage, pedantic lint, all tests passing.
Two things let it through:

- **A covered line is not a tested behaviour.**
  One test called `main()`, so the buggy line counted as covered, but it ran with the test program's own arguments, which ref refuses before running anything.
  Nothing read from real standard input.
- **Seams hide the one line that differs.**
  ADR-004 made hard-to-test code testable by injecting its outside resources (input, output, the stop signal, the Chrome program, the GitHub address, folders).
  Tests pass stand-ins, so the single line that passes the real resource is the only place the two can differ, and no test checks it.
  The MCP tests, for example, use an in-memory pipe, never stdio.

ADR-004 chose in-process tests on purpose ("inline tests call it in-process instead of starting the binary").
That choice stands for nearly every test; this ADR adds a thin layer on top of it.

## Decision

### 1. A few tests run ref as a process, inline

An inline test in `src/cli.rs` starts the test program again as a child process, with an environment variable set that names the case to run.
In the child, a test function sees the variable and calls `cli::main()` with the real standard input, output, error and signals.
Without the variable, that function does nothing, so a normal `cargo test` run is unaffected.
The parent test builds the child's command line and standard input, and checks the child's standard output, standard error and exit code.

This stays within ADR-004: the tests are inline, no `tests/*.rs` file is added, nothing is ignored, and no code is excluded from the count.
The binary target keeps `test = false`; the child is the test program itself.

The cases:

- `ref mcp`: send `initialize`, then one tool call, then close the input; the replies are read, the exit code is 0.
- `ref check-links --stdin`: URLs on real standard input are checked.
- `ref mcp` and `ref fetch` stopped with SIGTERM: exit code 130.

### 2. Every real resource is used for real by at least one test

A resource is real when `main` or a library default passes the actual one instead of an injected one: standard input, output and error, the stop signals, the search for the Chrome program, the GitHub address in `update`, the home folder for the saved profile.
For each, at least one test runs the real thing, in a child process where it would disturb the parent.
The list of resources and the test that covers each is kept in this ADR, so a new seam must add a line to it.

| Resource | Where it is passed | Test |
| --- | --- | --- |
| Standard input | `cli::main` | to be filled in when the tests are written |
| Standard output and error | `cli::main` | |
| Stop signals (Ctrl-C, SIGTERM) | `cli::main`, `shutdown_signal` | |
| Chrome program | `browser::StartOptions::found` | |
| Home folder | saved profile lookup | |
| GitHub address | `update` | |

### 3. What is not decided here

Whether the child's coverage reaches the gate is checked before this ADR is accepted: `cargo llvm-cov` must count lines run in a re-executed test program.
If it does not, the cases in rule 1 still run and still fail on a regression, but any line only they reach would show as uncovered; in that case this ADR is revised, not the gate.

## Consequences

### Positive

- A mistake in how the real resources are connected fails a test.
- The `ref mcp` hang would have failed at its first run.
- The list in rule 2 makes the "only line that differs" visible and owned.

### Negative

- A handful of tests start a process, so they are slower and need care with timeouts.
- The re-execution trick is less obvious to read than a plain call.
- The table in rule 2 is a convention; nothing enforces that a new seam adds a row.

## Alternatives considered

- **`tests/*.rs` with the binary** (the setup before 2026-11-17): breaks rule 3 of ADR-004, and its tests were never run for `mcp` over stdio anyway.
- **A shell script in CI** that starts `ref mcp`: runs only in CI, not in `cargo test`, and sits outside the coverage and lint gates.
- **Rely on review**: it did find this bug, but only because a reviewer read the line; a test finds it every run.
