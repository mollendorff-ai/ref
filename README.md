# Möllendorff Ref

> 📌 **R&D Prototype** — Interpret claims as hypotheses, not proven facts.

LLM-optimized reference toolkit.
JSON output for AI agents, not humans.

Bypasses bot protection (403/999) via headless Chrome.

## Why

```bash
# curl gets blocked
curl https://linkedin.com/...  # 999 Request Denied

# ref gets through
ref fetch https://linkedin.com/...  # JSON with content
```

## Install

From releases:

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

## Usage

```
ref <COMMAND>

Commands:
  fetch         Fetch URL and convert HTML to structured JSON (LLM-optimized)
  pdf           Extract text from PDF files to structured JSON
  init          Create references.yaml template
  scan          Scan markdown files for URLs, build references.yaml
  verify-refs   Verify references.yaml entries and update status
  check-links   Check URL health in markdown files or single URLs
  refresh-data  Extract live data from URLs (market sizes, pricing, statistics)
  update        Update to the latest version from GitHub releases

Options:
  -h, --help     Print help
  -V, --version  Print version
```

## Commands

### fetch

Fetch URL content as structured JSON.

```bash
ref fetch <url>
ref fetch <url> --raw      # Include raw HTML
ref fetch <url> --cookies  # Use browser cookies
```

### pdf

Extract text from PDF files to structured JSON.

```bash
ref pdf document.pdf
ref pdf *.pdf  # Multiple files
```

### init

Create a new references.yaml template.

```bash
ref init                    # Creates references.yaml
ref init -o refs.yaml       # Custom filename
ref init --force            # Overwrite existing
```

### scan

Scan markdown files for URLs, build/update references.yaml.

```bash
ref scan README.md docs/*.md
ref scan . --output refs.yaml
```

### verify-refs

Verify references.yaml entries, update status.

```bash
ref verify-refs references.yaml
ref verify-refs references.yaml --category research
ref verify-refs references.yaml --parallel 10
ref verify-refs references.yaml --dry-run
```

### check-links

Check URL health. Returns status codes.

```bash
ref check-links <file.md>           # All URLs in file
ref check-links --url <URL>         # Single URL
ref check-links --stdin             # From stdin
ref check-links -c 10 <file.md>     # 10 parallel checks
```

### refresh-data

Extract structured data (market sizes, stats, follower counts).

```bash
ref refresh-data --url <URL>
ref refresh-data <file.md>
```

### update

Self-update to the latest version from GitHub releases.

```bash
ref update            # Download and install latest
ref update --check    # Check for updates only
ref update --force    # Force reinstall current version
```

## Output

All commands output JSON to stdout, logs to stderr.

```bash
ref fetch https://example.com 2>/dev/null | jq .
```

## Requirements

- Chrome/Chromium (headless) - for fetch, check-links, verify-refs
- Rust toolchain (build from source only)

## License

[Elastic License 2.0](LICENSE) - Möllendorff Group Inc.
