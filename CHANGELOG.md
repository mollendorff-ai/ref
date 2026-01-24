# Changelog

All notable changes to Möllendorff Ref.

## [1.1.0] - 2025-01-02

Rebrand & Release Infrastructure.

### Changed

- **Rebranding: RoyalBit to Möllendorff Group Inc.** - Complete organizational rebrand.
  - Rebrand: ref-tools → ref (CLI) / Möllendorff Ref (product)
  - **Why rebrand?** The "RoyalBit" name (company founded 2006) was hijacked by unrelated cryptocurrency scammers:
    - UK FCA issued official warning (Oct 2024) about "Royalbit Miners" - unauthorized firm
    - Multiple fraudulent domains: royalbit.ltd (trust score 38/100), royalbit.top, royal-bit.club
    - Classic HYIP Ponzi schemes offering impossible returns (155-580% in days)
    - Sources: [FCA Warning](https://www.fca.org.uk/news/warnings/royalbit-miners), [Scam Detector](https://www.scam-detector.com/validator/royalbit-ltd-review/)

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
