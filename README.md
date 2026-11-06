# Möllendorff Ref

[![CI](https://github.com/mollendorff-ai/ref/actions/workflows/ci.yml/badge.svg)](https://github.com/mollendorff-ai/ref/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/mollendorff-ref.svg)](https://crates.io/crates/mollendorff-ref)
[![Tests](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/lctavares/d9c9e10e8e272d266ce29ae71e9d42e9/raw/ref-tests.json)](https://github.com/mollendorff-ai/ref/actions)
[![Logic coverage](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/lctavares/d9c9e10e8e272d266ce29ae71e9d42e9/raw/ref-coverage.json)](https://github.com/mollendorff-ai/ref/actions)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Renders web pages and PDFs into compact JSON for LLM agents.

> **Status:** a personal open-source project, usable but young (no known production users).
> It was built with AI coding assistants (Claude) under the maintainer's direction; changes go through pull requests and CI.
> See [Limitations](#limitations) before relying on it.

## The problem

LLM agents are bad at getting fresh web content:

- **Stale training data**: without explicit tooling, models answer from their training corpus, even when a search tool is available.
- **Raw HTML is noisy**: a fetched page is mostly navigation, scripts and markup, which costs tokens and hides the content.
- **JavaScript-heavy sites**: many sites return an empty HTML shell and load the content with JavaScript, so a plain HTTP fetch sees nothing.
- **Sign-ins and human checks**: much of what is worth reading sits behind a login, a subscription or an "I am human" check, which an agent's web tool cannot get past.

## What ref does

Chrome renders the page and waits for the network to go quiet (so JavaScript-loaded content arrives), then ref keeps the main content and outputs compact JSON.
With a [saved profile](#signed-in-pages-your-saved-profile), Chrome does this signed in to your sites, with the sessions you set up yourself.

```
┌──────────┐    ┌────────────┐    ┌───────────────┐    ┌──────────┐
│ Headless │ →  │ networkIdle│ →  │ Keep content, │ →  │ Compact  │
│ Chrome   │    │ wait (SPA) │    │ drop page UI  │    │ JSON out │
└──────────┘    └────────────┘    └───────────────┘    └──────────┘
```

### Output example

```bash
ref fetch https://example.com/report 2>/dev/null | jq .
```

```json
{
  "url": "https://example.com/report",
  "status": "ok",
  "http": 200,
  "title": "Quarterly report",
  "sections": [
    {
      "level": 1,
      "heading": "Quarterly report",
      "content": "Intro paragraph.\n\n- Revenue up 12%\n- Costs flat\n\n| Region | Q3 |\n| --- | --- |\n| North | 150 |\n\n> Strong quarter, said the CFO.\n\nLoose closing remark in a div."
    }
  ],
  "chars": 175
}
```

- Paragraphs, lists, tables, block quotes and definition lists are kept in document order; lists and tables come out as Markdown.
- Navigation, headers, footers, asides and forms outside the article are dropped (`--raw` keeps them).
- Code blocks go to a separate `code` array with the detected language.
- Null fields and empty arrays are omitted.

### Status and HTTP code

`status` is one of `ok`, `dead`, `redirect`, `paywall` or `login`; `http` is the HTTP status of the main document as Chrome received it.
Any 4xx or 5xx response is `dead`.
A page is `paywall` or `login` only when there is a real sign of a wall: `isAccessibleForFree: false` structured data, a wall overlay element, or wall wording or a password field on a page with little readable text.

### Size limits are reported, never silent

By default each section is limited to 5,000 characters and a page to 50 sections; links are limited to 50.
A cut section has `"truncated": true`, the page has `"truncated": true`, and an alert says which limit applied and how to lift it:

```bash
ref fetch --max-chars 0 --max-sections 0 https://example.com/long-article   # no limits
```

Other alerts say when nothing readable was found, when `--selector` matched nothing (ref then returns the main content), and when a URL turned out to be a PDF (ref then extracts it as `ref pdf` would).

Extract one element with `--selector`:

```bash
ref fetch --selector "#pricing-table" https://example.com 2>/dev/null | jq .
```

### PDF extraction

```bash
ref pdf document.pdf 2>/dev/null | jq .
ref pdf https://example.com/report.pdf 2>/dev/null | jq .
```

Same JSON structure, plus table detection (whitespace column analysis, header inference, Markdown output) and heading detection (numbered sections, Roman numerals, ALL CAPS, academic and legal formats).

## Commands

| Command | Description |
|---------|-------------|
| `ref fetch <url>` | Render page via Chrome, output structured JSON |
| `ref pdf <file\|url>` | Extract text and tables from PDFs |
| `ref scan <files>` | Find URLs in Markdown, build references.yaml |
| `ref verify-refs <file>` | Check reference entries, update status |
| `ref check-links <file>` | Validate URL health (HTTP status codes) |
| `ref refresh-data --url <url>` | Pull amounts and percentages from a page's main content |
| `ref login [urls]` | Open Chrome with ref's saved profile to sign in to your sites once |
| `ref init` | Create references.yaml template |
| `ref update` | Self-update from GitHub releases (SHA-256 verified) |
| `ref mcp` | Start MCP server (JSON-RPC 2.0 over stdio) |

Exit codes: `0` when everything succeeded, `1` when any URL or file failed, `2` when the command itself failed.
Progress goes to stderr; JSON goes to stdout.

## Signed-in pages: your saved profile

Many useful pages sit behind a sign-in, a subscription or an "I am human" check.
ref reads them with your own sessions, which you set up once in a normal Chrome window.

### First time

1. Run `ref login`, optionally with the sites you want to open:

   ```bash
   ref login https://www.example-news.com https://portal.example.org
   ```

   A normal, visible Chrome window opens with ref's saved profile.
   Without URLs it opens an empty window.
2. In that window, sign in to your sites, accept their cookie banners and pass any "I am human" checks yourself.
3. Quit that Chrome completely (Cmd-Q on a Mac, or close every window elsewhere).
   Chrome writes cookies to disk every few seconds and on quit; quitting makes sure everything is saved and frees the profile for ref.
4. Use ref as usual. `ref fetch`, `ref pdf` and the MCP tools now run headless Chrome with that profile and see those sites signed in.
   ref prints `Using saved profile <folder>` on stderr, so you can tell which profile a run used.

### Later

- **A session expired** (a page comes back as `login` or `paywall`, or with much less content than you see in your browser): run `ref login <that site>` again, sign in, quit.
- **Adding a site**: the same, `ref login <new site>`.
- **Upgrading from 1.7 or earlier**: older versions kept cookies only by accident, in a temporary folder, so run `ref login` once to set up the saved profile.

### Where it lives and how it behaves

- The saved profile is the folder `.ref/chrome-profile` in your home folder (`~/.ref/chrome-profile` on macOS and Linux, `%USERPROFILE%\.ref\chrome-profile` on Windows).
  ref uses it automatically once it exists and never deletes it.
- It is separate from your everyday Chrome, which keeps running as usual.
  ref cannot use your everyday profile: since version 136, Chrome refuses remote control of it to protect its cookies, and ref says so if you point it there.
- Only one Chrome can use the profile at a time: quit the `ref login` window before fetching, and stop `ref mcp` before running `ref login`.
  ref says which process holds it if you forget.
- `REF_CHROME_PROFILE=<folder>` uses another saved profile (for example one per project); `REF_CHROME_PROFILE=fresh` runs with an empty, temporary profile instead, deleted afterwards.
- With the saved profile, ref sends Chrome's normal user agent, because sessions and human-check passes are often tied to it.
- Treat the profile folder like a password: it holds your sessions.
  An agent using ref through MCP browses those sites as you, so only connect agents you trust.

`ref fetch --cookies cookies.txt` sends cookies from a Netscape-format cookie file instead, for a one-off run.
Treat that file like a password too.

## MCP Server Mode

`ref mcp` starts a persistent MCP server over stdio.
AI applications call tools directly. `ref_fetch` reuses one browser between calls; the other browser tools start one per call.

Six tools: `ref_fetch`, `ref_pdf`, `ref_check_links`, `ref_scan`, `ref_verify_refs`, `ref_refresh_data`.

**Claude Code** (`.mcp.json` in project root):

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

**Claude Desktop** (`~/Library/Application Support/Claude/claude_desktop_config.json`):

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

MCP tools only read and write files inside the directory the server was started in, or the folder given with `ref mcp --root <folder>`.
`ref_scan` writes only `.yaml` or `.yml` files, never overwrites a file that is not already a references file, and refuses hard-linked files; outputs are written to a temporary file and renamed into place.
If that folder is the filesystem root, file tools are refused.

See [docs/mcp-integration.md](docs/mcp-integration.md) for the full setup guide and tool reference.

## Use with agent orchestrators

ref can be the web tool for [Asimov](https://github.com/mollendorff-ai/asimov), a vendor-neutral orchestrator for AI coding CLIs (Claude Code, Gemini CLI, Codex CLI).
Asimov's `freshness` protocol can tell agents to prefer `ref fetch` over built-in search, so they read current pages instead of answering from training data.
Whether to forbid other tools entirely is your call; check that ref works for the sites you need first.

## Security

- Only `http` and `https` URLs are opened; `file://`, `data:` and other schemes are refused.
- Chrome runs with its sandbox unless ref runs as root, `REF_CHROME_NO_SANDBOX=1` is set, or Chrome cannot start with it (ref then retries without it and prints a warning).
- Without a saved profile, each run uses its own temporary Chrome profile, deleted afterwards (also after Ctrl-C or SIGTERM).
  If a run is killed outright (SIGKILL), the next run on Linux stops its leftover Chrome and removes its profile.
  A saved profile (see above) is never deleted.
- ref turns off Chrome's optional background services (sync, metrics, safe-browsing updates, translation and others).
  Chrome still makes a few calls of its own to Google, such as component update and account checks, that these switches do not stop; block them at your firewall or proxy if that matters to you.
- With a temporary profile, the user agent is Chrome's own plus `mollendorff-ref/<version>`; with your saved profile it is Chrome's own.
  ref reuses the sessions and checks you completed yourself; it does not solve CAPTCHAs or disguise itself in other ways.

## Limitations

- **Bot walls and CAPTCHAs**: ref does not solve them. With your saved profile, a check you passed in `ref login` usually carries over until it expires; without it, sites that block automated browsers will block ref too, and those pages come back as `dead` or with little content.
- **Content heuristics**: main-content selection (`main`, `article`, then common class names, then `body`) can miss content on unusual layouts; use `--selector` or `--raw`.
- **Late rendering**: content that appears only after a timer, with no network activity, can be missed; ref waits for the network to go quiet, not for arbitrary scripts.
  In tests, content added 200 ms after load was kept and content added 2.5 s after load was missed.
- **Web components**: text in open shadow roots is read; closed shadow roots cannot be.
- **Paywall and login detection** are heuristics and can be wrong either way.
- **PDFs**: text-based PDFs only (no OCR); table detection relies on column alignment.
- **Platforms**: CI tests Linux; macOS and Windows builds are produced but not tested end to end.

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

Each release includes `checksums.txt` (SHA-256) to verify the download.

From crates.io:

```bash
cargo install mollendorff-ref
```

## Requirements

- Chrome or Chromium (for fetch, check-links, verify-refs, refresh-data)
- Rust toolchain (build from source only)

## License

[MIT](LICENSE)
