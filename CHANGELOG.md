# Changelog

All notable changes to Möllendorff Ref.

## [1.5.0] - 2026-02-22

MCP Server Mode — persistent JSON-RPC 2.0 server for AI tool integration.

### Added

- **`ref mcp`** subcommand: starts MCP server over stdio with persistent browser pool
- **6 MCP tools**: `ref_fetch`, `ref_pdf`, `ref_check_links`, `ref_scan`, `ref_verify_refs`, `ref_refresh_data`
- **MCP integration guide**: `docs/mcp-integration.md` with setup for Claude Code, Claude Desktop, VS Code
- **ADR-001 updated**: Status changed from Deferred to Accepted

### Changed

- Core functions (`fetch_one`, `extract_pdf`, `scan_files`, `verify_refs_core`) promoted to `pub(crate)` for MCP reuse
- `scan.rs` and `verify_refs.rs` refactored: core logic extracted from CLI print wrappers

### Dependencies

- **rmcp** 0.16 (Anthropic official MCP SDK)
- **schemars** 0.8 (JSON Schema generation for tool parameters)
- **tracing** 0.1 + **tracing-subscriber** 0.3 (structured logging to stderr)

## [1.4.0] - 2026-02-22

Dependency hygiene and automation.

### Added

- **Dependabot**: Weekly automated PRs for cargo and github-actions dependencies
- **cargo audit**: Zero known vulnerabilities confirmed

### Changed

- **regex** 1.11 -> 1.12
- **reqwest** 0.13.1 -> 0.13.2
- **assert_cmd** 2.0 -> 2.1
- **tempfile** 3.14 -> 3.25

## [1.3.1] - 2026-02-21

Dependency upgrades and quality gate fixes.

### Changed

- **chromiumoxide** 0.7 -> 0.9 (tokio runtime now built-in)
- **scraper** 0.22 -> 0.25
- **html2text** 0.14 -> 0.16
- **pdf-extract** 0.8 -> 0.10
- **thiserror** 1.0 -> 2.0
- 72 transitive dependencies updated via `cargo update`

### Fixed

- Pre-commit coverage gate now excludes IO modules per ADR-003 (enforces 100% on pure-logic modules only)
- Markdown lint config added (`.markdownlint.jsonc`) — disables rules incompatible with project style
- Blank-line violations in CHANGELOG and ADRs

## [1.3.0] - 2026-02-16

Open source readiness.

### Changed

- **License**: Elastic 2.0 to MIT
- **README**: Full rewrite — token optimization value prop, pipeline diagram, output example, Asimov orchestration link
- **Cargo.toml**: Updated description and license

### Fixed

- All 77 clippy pedantic warnings resolved with zero `#[allow(...)]` bypasses
- Safe integer casts (replace `as` with `try_from`, `abs_diff`, integer cross-multiplication)
- Added `# Errors` and `# Panics` doc sections to all public functions
- Removed unused `async` from `extract_pdf`
- Windows build: `#[cfg(unix)]` guard on `PermissionsExt`, `.zip` archive detection for Windows assets
- CI clippy enforces `--all-targets` (lints tests too, not just library)
- Migrated from deprecated `cargo_bin()` to `cargo_bin_cmd!()` macro in integration tests
- Chrome-dependent test moved behind `#[ignore]` per ADR-003

### Added

- ADR-003: Test strategy — unit tests for pure logic, `#[ignore]` e2e tests for Chrome-dependent code
- **GitHub Actions**: CI workflow with clippy pedantic, unit tests (stable + beta), e2e tests (Chrome), release build
- **GitHub Actions**: Auto-release on version bump (tag + multi-platform binaries + crates.io publish)
- **GitHub Actions**: E2E test job — runs `#[ignore]` tests with Chrome on ubuntu runners

### Removed

- R&D prototype disclaimer
- Legacy asimov protocol files (asimov.json, green.json, warmup.json, etc.)

## [1.2.0] - 2026-01-26

Enhanced extraction & SPA support.

### Added

- **Network idle wait for SPAs**: `ref fetch` now waits for `networkIdle` (no requests for 500ms) before extracting content. This ensures SPAs load their dynamic content before extraction. Falls back to timeout for sites with persistent connections.

- **PDF table extraction**: Detects tables via whitespace column alignment
  - Finds consistent column boundaries across rows
  - Header row detection (non-numeric first row, title case)
  - Outputs structured `tables[]` array with `headers`, `rows`, and `markdown`
- **Improved PDF heading detection**: Multiple pattern types with confidence scoring
  - Numbered sections: `1.`, `1.2`, `1.2.3` with correct level inference
  - Roman numerals: `I.`, `IV.`, `XIV.`
  - Structural keywords: `Chapter 3`, `Section 2.1`, `Appendix A`
  - Academic keywords: Abstract, Introduction, Methods, Results, etc.
  - Legal patterns: WHEREAS, DEFINITIONS, GOVERNING LAW
  - ALL CAPS headings
  - Page reference stripping: `Introduction ...... 15` → `Introduction`

### Changed

- PDF output now includes `tables` field (empty array if no tables detected)
- Heading detection uses confidence threshold (0.5+) instead of simple regex

## [1.1.0] - 2025-01-02

Rebrand & Release Infrastructure.

### Changed

- **Renamed from RoyalBit to Möllendorff AI**

### Added

- Cargo.toml: crates.io publishing metadata
- GitHub Actions: CI workflow (test, lint, build)
- GitHub Actions: Release workflow (multi-arch)
- Targets: linux-x64, linux-arm64, macos-x64, macos-arm64, windows-x64
- `update` command: self-update from GitHub releases
- Updated LICENSE, README, schemas for new branding

## [1.0.0] - 2024-12-31

Full LLM reference toolkit.

### Added

- `pdf` command: extract text from PDF files
- `pdf` command: output same JSON structure as fetch

## [0.9.0] - 2024-12-30

### Added

- `init` command: create references.yaml template
- `scan` command: extract URLs from markdown files
- `scan` command: dedupe, track cited_in per reference
- `scan` command: infer categories from file paths

## [0.8.0] - 2024-12-29

### Added

- deploy-kveldulf target (remote build)
- Simplified Makefile

## [0.7.1] - 2024-12-28

### Added

- `check-links` command: compact JSON output
- `refresh-data` command: compact JSON output

## [0.7.0] - 2024-12-27

### Added

- `fetch` command: structured sections[], links[], code[] output
- Content cleaning (strip nav/footer/aside)
- `--raw` and `--cookies` flags

## [0.6.0] - 2024-12-26

### Added

- `verify-refs` command with references.yaml schema
- Status detection (ok/dead/redirect/paywall/login)
- JSON Schema v1 (schemas/references.v1.schema.json)
