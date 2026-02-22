# Möllendorff Ref

[![CI](https://github.com/mollendorff-ai/ref/actions/workflows/ci.yml/badge.svg)](https://github.com/mollendorff-ai/ref/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/mollendorff-ref.svg)](https://crates.io/crates/mollendorff-ref)
[![Tests](https://img.shields.io/badge/tests-48_passing-brightgreen)](https://github.com/mollendorff-ai/ref)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Renders web pages and PDFs into token-optimized JSON for LLM agents.

## The problem

LLM agents are bad at getting fresh web content:

- **Stale training data** — without explicit tooling, models default to their training corpus, returning confident answers from months-old snapshots.
Even with web search enabled, models can weight stale training knowledge over fresh results.
- **Bot protection** — `curl`, `wget`, and built-in web tools get blocked (403/999) by most sites.
When they do get through, they return raw HTML — 10,000-50,000 tokens of navigation, ads, and markup noise.
- **SPA blindness** — modern sites return empty HTML shells.
The actual content loads via JavaScript after the initial response.

## What ref does

Chrome renders the page, waits for all network requests to settle (SPAs included), then ref extracts structured content and outputs compact JSON.

**100 KB of rendered HTML becomes 1-5 KB of structured JSON.**

```
┌──────────┐    ┌───────────┐    ┌──────────┐    ┌──────────┐
│ Headless │ →  │ networkIdle│ →  │ Strip    │ →  │ Compact  │
│ Chrome   │    │ wait (SPA) │    │ nav/ads  │    │ JSON out │
└──────────┘    └───────────┘    └──────────┘    └──────────┘
```

### Output example

```bash
ref fetch https://example.com 2>/dev/null | jq .
```

```json
{
  "url": "https://example.com",
  "status": "ok",
  "title": "Example Domain",
  "sections": [
    {
      "level": 1,
      "heading": "Example Domain",
      "content": "This domain is for use in illustrative examples..."
    }
  ],
  "links": [
    { "text": "More information...", "url": "https://www.iana.org/domains/example" }
  ],
  "chars": 1256
}
```

Null fields and empty arrays are omitted.
Sections are capped (200 char headings, 2,000 char content).
Code blocks include detected language.
Status detects paywalls, login walls, and dead links.

### PDF extraction

```bash
ref pdf document.pdf 2>/dev/null | jq .
```

Same JSON structure, plus table detection (whitespace column analysis, header inference, markdown output) and heading detection (numbered sections, Roman numerals, ALL CAPS, academic/legal formats).

## Commands

| Command | Description |
|---------|-------------|
| `ref fetch <url>` | Render page via Chrome, output structured JSON |
| `ref pdf <file>` | Extract text and tables from PDFs |
| `ref scan <files>` | Find URLs in markdown, build references.yaml |
| `ref verify-refs <file>` | Check reference entries, update status |
| `ref check-links <file>` | Validate URL health (HTTP status codes) |
| `ref refresh-data --url <url>` | Extract live data (market sizes, stats) |
| `ref init` | Create references.yaml template |
| `ref update` | Self-update from GitHub releases |

## AI orchestration

ref is designed to work with [Asimov](https://github.com/mollendorff-ai/asimov), a vendor-neutral orchestrator for AI coding CLIs (Claude Code, Gemini CLI, Codex CLI).

Asimov's `freshness` protocol forces agents to use `ref fetch` for all web content instead of relying on built-in search tools or training data:

```json
{
  "rule": "MUST use ref fetch <url> for all web fetching. NEVER use WebSearch or WebFetch."
}
```

This ensures agents work with current, verified content rather than stale or hallucinated sources.

## Install

From [releases](https://github.com/mollendorff-ai/ref/releases):

```bash
# macOS (Apple Silicon)
curl -L https://github.com/mollendorff-ai/ref/releases/latest/download/ref-aarch64-apple-darwin.tar.gz | tar xz
sudo mv ref /usr/local/bin/

# macOS (Intel)
curl -L https://github.com/mollendorff-ai/ref/releases/latest/download/ref-x86_64-apple-darwin.tar.gz | tar xz
sudo mv ref /usr/local/bin/

# Linux (x64)
curl -L https://github.com/mollendorff-ai/ref/releases/latest/download/ref-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo mv ref /usr/local/bin/

# Linux (ARM64)
curl -L https://github.com/mollendorff-ai/ref/releases/latest/download/ref-aarch64-unknown-linux-musl.tar.gz | tar xz
sudo mv ref /usr/local/bin/
```

From crates.io:

```bash
cargo install mollendorff-ref
```

## Requirements

- Chrome or Chromium (for fetch, check-links, verify-refs)
- Rust toolchain (build from source only)

## License

[MIT](LICENSE)
