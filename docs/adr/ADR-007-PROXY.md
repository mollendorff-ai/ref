# ADR-007: Proxy

**Status:** Accepted (2026-11-22)
**Date:** 2026-11-22
**Author:** Claude (AI assistant, under the maintainer's direction)

---

## Context

ref sends traffic from two places: Chrome, which loads pages, and ref itself, which downloads PDFs.
Both already follow the proxy variables of the environment (`http_proxy` and the like), found by experiment on 2026-11-22: Chrome on Linux reads them with no flag, and the download library reads them by default.
What was missing was a way to say, for one run or one MCP server, "use this proxy", that wins over the environment and reaches both places.

## Decision

### 1. One address, checked once

`--proxy` and `REF_PROXY` are parsed by `proxy::parse_proxy` into one `Proxy` value, `scheme://host:port`, before Chrome starts.
The option wins over the variable, and both win over the proxy variables of the environment: Chrome's `--proxy-server` beats them, and an explicit proxy given to the download library switches the environment's off.

### 2. No user name or password

Chrome's `--proxy-server` takes no credentials.
Answering a proxy's login request means handling Chrome's authentication challenge on every tab, a second path through the code that opens tabs (ADR-005), and a password on a command line is visible to every user of the machine.
An address with credentials is refused, with a message that points to a proxy that needs none, such as an SSH tunnel.
Proxies that need a login can be a later change; nothing here prevents it.

### 3. Schemes: `http`, `https`, `socks5`, `socks5h`

A SOCKS5 proxy always looks names up itself, in Chrome (which has no `socks5h`) and in the downloads (where `socks5` looks names up locally).
ref therefore sends `socks5` to Chrome for both spellings and `socks5h` to the downloads for both, so the proxy sees the same names from every part of ref and no lookup leaves the machine.

### 4. Addresses on this machine are not proxied

Chrome never sends `localhost`, `127.0.0.1` and `::1` through a proxy.
The downloads leave the same three out, so a page and the PDF it links to go the same way.

### 5. The proxy belongs to the Chrome, not to the call

A Chrome is started with its proxy and keeps it.
`ref mcp` shares one Chrome across every call (ADR-005), so its proxy is set for the server (`ref mcp --proxy`) and a call cannot choose one.
Letting an agent choose where its traffic goes is also a permission that belongs to the person who runs the server.

### 6. A proxy that fails is named, and is not a fact about the site

Chrome answers a proxy that cannot be reached with `ERR_PROXY_*` or `ERR_TUNNEL_CONNECTION_FAILED` while it is itself alive, so under ADR-005 rule 3 it is the page's failure and not `ChromeFailed`.
The page is `dead` with `PROXY_FAILED: <Chrome's error> (proxy <address>)`, so a reader does not take it for a dead site.
When ref passed no proxy and Chrome used the environment's, the alert says `from the environment`.

## Testing

The test server answers as an HTTP proxy (requests in absolute form are routed by their path) and a small SOCKS5 server forwards to it.
Pages are fetched from a host name that does not exist, so a page that loads has come through the proxy.
Every place that passes the proxy on has a test that fails when the proxy is dropped there (checked by removing it).
`REF_PROXY` is tested in a child process, so the variable is not set in the test run itself.

## Not covered

- `ref update` and `ref scan`: they make no page requests (`ref update` talks to GitHub with the environment's defaults).
- Proxies that need a login (decision 2).
- macOS and Windows: the SOCKS and HTTP flag is Chrome's own, but only Linux has been run.
