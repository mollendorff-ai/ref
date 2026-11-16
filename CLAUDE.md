# Ref

Rust CLI and MCP server that renders web pages and PDFs into compact JSON for LLM agents.
Read `.asimov/project.yaml` (conventions, coding standards) and `.asimov/roadmap.yaml` (what to build) before starting work.

## Rules (non-negotiable)

These hold for every change, with no exceptions and no waivers.

1. **100% line coverage of all code, nothing skipped.**
   No `--ignore-filename-regex`, no `#[ignore]`, no `#[coverage(off)]`, no `cfg` tricks that hide code from the count.
   Code that is hard to test is changed so it can be tested (inject the URL, the folder, the clock); it is not excluded.
2. **Pedantic linting and formatting, nothing skipped.**
   `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings -W clippy::pedantic` must pass.
   No new `#[allow(clippy::...)]`; fix the code instead.
   The few existing ones each carry a reason on the same line.
3. **All tests inline.**
   Tests live in `#[cfg(test)] mod tests` inside the file they test.
   No `tests/*.rs` files.
   Test data (HTML, PDFs) may live in `tests/fixtures/`.
4. **No source file over 1,500 lines**, tests included.
   Before a file passes 1,500 lines, split it into a module folder (`foo/mod.rs` plus submodules) along its real seams.

Tests that need Chrome run in plain `cargo test`: Chrome is a requirement of the tool, so it is a requirement of the tests.

## Before every commit

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings -W clippy::pedantic
cargo llvm-cov --fail-under-lines 100
find src -name '*.rs' -exec wc -l {} + | awk '$2 != "total" && $1 > 1500 {print; bad=1} END {exit bad}'
```

All four must pass.
If one fails, fix the cause; never weaken the check.

## Status

The repo does not meet rules 1 and 3 yet (78% coverage over all code, two files in `tests/`).
The work to get there is the first item in `.asimov/roadmap.yaml` (`STRICT_RULES`); remove this section when it is done.

## Docs

One sentence per line in Markdown, for clean diffs.
The README states only what the code does; check it against the code when behaviour changes.
