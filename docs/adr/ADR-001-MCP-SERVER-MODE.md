# ADR-001: MCP Server Mode

**Status:** Deferred (backlog)
**Date:** 2026-01-26
**Author:** Claude Opus 4.5 (Principal Autonomous AI)

---

## Deferral Rationale

Moved to backlog on 2026-01-26:

1. **No iOS support** - MCP not available on Claude iOS app; remote MCP (HTTP) not yet supported on mobile
2. **CLI works fine** - `ref fetch <url>` via Bash tool is already integrated and functional
3. **Marginal benefit** - Browser pool persistence benefit is minimal for typical 1-5 URL fetches
4. **Complexity cost** - Adding rmcp dependency (+300KB) for feature with limited reach

Revisit when: iOS gets MCP support, or Claude Desktop becomes primary workflow.

---

## Context

AI applications (Claude Desktop, Claude Code, VS Code extensions) need standardized ways to interact with external tools.
The Model Context Protocol (MCP) is Anthropic's open standard for this purpose.

Currently, `ref` is invoked via shell commands:

```bash
ref fetch https://example.com
```

This works but has limitations:

1. **Process overhead**: Each invocation spawns a new process and browser instance
2. **No persistence**: Browser pool restarts for every command
3. **Shell escaping**: URLs with special characters require careful quoting
4. **No discovery**: AI models can't introspect available commands programmatically

## Decision

**Implement MCP server mode using the `rmcp` crate (official Rust SDK).**

```bash
ref mcp  # Start MCP server on stdio
```

## Rationale

### 1. Crate Selection

Three Rust MCP implementations were evaluated:

| Crate | Downloads | Version | Maintainer | Status |
|-------|-----------|---------|------------|--------|
| **rmcp** | 2,875,904 | 0.14.0 | Anthropic (official) | Active (3 days ago) |
| rust-mcp-sdk | 66,664 | 0.9.0 | Community | Active |
| mcp-attr | 6,120 | 0.0.7 | Community | Stale (8 months) |

**I chose `rmcp` because:**

1. **Official SDK**: Maintained by Anthropic under `modelcontextprotocol` org
2. **Battle-tested**: 2.8M downloads, 2,880 GitHub stars
3. **Feature-complete**: All transports (stdio, HTTP), OAuth2, task lifecycle
4. **Macro ergonomics**: `#[tool]` attribute generates JSON schemas automatically
5. **Protocol currency**: Always first to support new MCP features

`rust-mcp-sdk` is a viable alternative with excellent docs, but tracking an unofficial implementation creates risk when the official SDK exists.

### 2. Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    AI Application                        │
│              (Claude Desktop, Claude Code)               │
└─────────────────────┬───────────────────────────────────┘
                      │ JSON-RPC 2.0 (stdio)
                      │
┌─────────────────────▼───────────────────────────────────┐
│                    ref mcp                               │
│  ┌─────────────────────────────────────────────────┐    │
│  │                   Tool Router                    │    │
│  │  ┌─────────┐ ┌─────────┐ ┌───────────────────┐  │    │
│  │  │  fetch  │ │   pdf   │ │   check_links     │  │    │
│  │  └─────────┘ └─────────┘ └───────────────────┘  │    │
│  │  ┌─────────┐ ┌─────────┐ ┌───────────────────┐  │    │
│  │  │  scan   │ │ verify  │ │   refresh_data    │  │    │
│  │  └─────────┘ └─────────┘ └───────────────────┘  │    │
│  └─────────────────────────────────────────────────┘    │
│  ┌─────────────────────────────────────────────────┐    │
│  │              Shared Browser Pool                 │    │
│  │        (persistent across tool calls)            │    │
│  └─────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

### 3. Tools to Expose

| Tool | Description | Primary Use |
|------|-------------|-------------|
| `fetch` | Fetch URL(s), return structured JSON | Web research |
| `pdf` | Extract text from PDF files | Document analysis |
| `check_links` | Verify URL health (200/404/etc) | Link validation |
| `scan` | Find URLs in markdown files | Reference discovery |
| `verify_refs` | Validate references.yaml entries | Citation checking |
| `refresh_data` | Extract live data from URLs | Market research |

### 4. Protocol Benefits

MCP provides:

- **Persistent server**: Browser pool stays warm between calls
- **Tool discovery**: `tools/list` returns available operations with schemas
- **Structured I/O**: JSON schemas for inputs, consistent JSON outputs
- **Standard protocol**: JSON-RPC 2.0 over stdio (no port management)

### 5. Implementation Complexity

```toml
[dependencies]
rmcp = { version = "0.14", features = ["server", "macros", "transport-io"] }
```

Estimated impact:

- Binary size: +200-300KB
- New code: ~400 lines (src/mcp.rs)
- Reuse: Existing command logic unchanged

## Consequences

### Positive

- First-class AI integration (Claude Desktop, Claude Code)
- Performance improvement from persistent browser pool
- Future-proof (MCP is Anthropic's standard, actively developed)
- Single binary for CLI and MCP modes

### Negative

- Additional dependency (~300KB)
- Must maintain tool schemas alongside CLI args
- 91 open issues on rmcp (active development = some rough edges)

### Neutral

- CLI interface unchanged
- Output format unchanged (still JSON)
- No breaking changes for existing users

## Alternatives Considered

1. **HTTP server mode** (`ref serve --port 8080`)
   - Rejected: Requires port management, network overhead for local use
   - MCP stdio is the standard for CLI tools

2. **rust-mcp-sdk instead of rmcp**
   - Rejected: Unofficial implementation creates maintenance risk
   - Would need to track compatibility with official protocol updates

3. **Custom JSON-RPC protocol**
   - Rejected: Reinventing the wheel, no ecosystem compatibility

4. **Keep CLI only**
   - Rejected: Loses browser pool persistence benefit, no tool discovery

## Configuration

Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`):

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

## References

- [MCP Specification](https://modelcontextprotocol.io/)
- [rmcp Crate](https://crates.io/crates/rmcp) - 2.8M downloads
- [rmcp GitHub](https://github.com/modelcontextprotocol/rust-sdk) - 2,880 stars
- [MCP Rust Quickstart](https://modelcontextprotocol.io/docs/develop/build-server)

---

*This decision reflects engineering judgment prioritizing official implementations and ecosystem compatibility over community alternatives.*

— Claude Opus 4.5, Principal Autonomous AI
