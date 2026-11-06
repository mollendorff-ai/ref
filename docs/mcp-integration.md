# MCP Integration Guide

Run `ref mcp` to start a persistent MCP server over stdio.
The browser pool stays warm between tool calls — no Chrome restart per request.

## Setup

### Claude Code

Add `.mcp.json` to your project root:

```json
{
  "mcpServers": {
    "ref": {
      "command": "ref",
      "args": ["mcp"]
    }
  }
}
```

### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "ref": {
      "command": "/usr/local/bin/ref",
      "args": ["mcp"]
    }
  }
}
```

### VS Code (Copilot / Continue)

Add to `.vscode/mcp.json`:

```json
{
  "servers": {
    "ref": {
      "command": "ref",
      "args": ["mcp"]
    }
  }
}
```

## Tools

All tools return JSON in `CallToolResult` content.
Error responses use MCP error codes (invalid params, internal error).

File paths (`ref_pdf` local files, `ref_scan`, `ref_verify_refs`, `ref_refresh_data`'s `file`) must stay inside the workspace: the directory the server was started in, or `ref mcp --root <folder>`.
Absolute paths elsewhere, `..`, symlinks that lead outside (including files matched by a `ref_scan` glob) and symlinks to missing files are refused with an invalid-params error.
When the workspace is the filesystem root, file tools are refused.

`parallel` and `concurrency` must be between 1 and 20.
The server keeps one browser with up to 16 tabs; each call uses at most its own `parallel` value, and every tab is closed when its page is done.
Only `http` and `https` URLs are fetched.

### ref_fetch

Render web pages via headless Chrome and return structured JSON.
URLs in one call are fetched concurrently, up to `parallel` tabs.
Each page reports `status`, `http` and, when content was cut, `"truncated": true`.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `urls` | string[] | (required) | URLs to fetch |
| `parallel` | number | 4 | Pages fetched at once (1-20) |
| `timeout` | number | 30000 | Timeout per URL in ms |
| `raw` | boolean | false | Skip content cleaning |
| `selector` | string | - | CSS selector to extract a specific element, skipping content heuristics |
| `max_chars` | number | 5000 | Characters per section before it is cut and marked truncated (0 = no limit) |
| `max_sections` | number | 50 | Sections per page before the rest are dropped and the page is marked truncated (0 = no limit) |

```json
{
  "urls": ["https://example.com"],
  "timeout": 15000
}
```

### ref_pdf

Extract text, tables, and headings from PDF files.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `files` | string[] | (required) | PDF paths inside the working directory, or http(s):// URLs |

```json
{
  "files": ["docs/report.pdf"]
}
```

### ref_check_links

Validate URL health using headless Chrome.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `urls` | string[] | (required) | URLs to check |
| `concurrency` | number | 5 | Parallel tabs (1-20) |
| `timeout` | number | 15000 | Timeout per URL in ms |
| `retries` | number | 1 | Retry count on failure |

```json
{
  "urls": ["https://example.com", "https://example.org"],
  "concurrency": 3
}
```

### ref_scan

Scan markdown files for URLs and build/update references.yaml.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `files` | string[] | (required) | File paths or glob patterns |
| `output` | string | "references.yaml" | Output file path |
| `merge` | boolean | true | Merge with existing file |

```json
{
  "files": ["docs/*.md"],
  "output": "references.yaml"
}
```

### ref_verify_refs

Verify references.yaml entries by fetching each URL.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `file` | string | (required) | Path to references.yaml |
| `parallel` | number | 4 | Parallel browser tabs (1-20) |
| `category` | string[] | (all) | Filter by categories |
| `timeout` | number | 30000 | Timeout per URL in ms |
| `dry_run` | boolean | false | Don't write changes |

```json
{
  "file": "references.yaml",
  "category": ["research"],
  "dry_run": true
}
```

### ref_refresh_data

Extract live data (amounts, percentages, statistics) from URLs.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `url` | string | - | Single URL to extract from |
| `file` | string | - | Markdown file to extract URLs from |
| `timeout` | number | 20000 | Timeout per URL in ms |

One of `url` or `file` is required.

```json
{
  "url": "https://www.statista.com/statistics/123"
}
```

## Troubleshooting

**Chrome not found**: Ensure Chrome or Chromium is installed and in the standard location.
The server logs to stderr — check `ref mcp` stderr output for browser launch errors.

**No tools in client**: Verify the MCP config uses `"command": "ref"` and `"args": ["mcp"]`.
Run `ref mcp` manually and send an initialize request to confirm:

```json
{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}},"id":1}
```

**Debug logging**: Set `RUST_LOG=debug` environment variable for verbose output on stderr.

## Architecture

See [ADR-001](adr/ADR-001-MCP-SERVER-MODE.md) for design rationale.
