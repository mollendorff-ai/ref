# Changelog

All notable changes to Möllendorff Ref.

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

- **Renamed from RoyalBit to Möllendorff Group Inc.**

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
