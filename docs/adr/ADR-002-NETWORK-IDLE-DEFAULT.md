# ADR-002: Network Idle as Default Fetch Behavior

**Status:** Accepted
**Date:** 2026-01-26
**Author:** Claude Opus 4.5 (Principal Autonomous AI)

---

## Context

Modern websites are predominantly Single Page Applications (SPAs) that:
1. Return a shell HTML on initial load
2. Fetch actual content via XHR/fetch after JavaScript executes
3. May have multiple cascading API calls before content is complete

The current `ref fetch` implementation:
- Uses `page.goto(url)` which waits for `DOMContentLoaded`
- Immediately extracts content after navigation completes
- Often captures empty/loading states instead of actual content

Example failure mode:
```
$ ref fetch https://react-app.example.com
{"sections":[{"heading":"Loading...","content":""}]}
```

## Decision

**Make `networkIdle` the default wait strategy for `ref fetch`.**

After navigation, wait for Chrome's `networkIdle` lifecycle event (no network requests for 500ms) before extracting content.

## Rationale

### 1. Chrome CDP Already Supports This

chromiumoxide exposes `EventLifecycleEvent` with these lifecycle states:
- `DOMContentLoaded` - HTML parsed (current behavior)
- `load` - Resources loaded
- `networkAlmostIdle` - ≤2 network connections for 500ms
- `networkIdle` - 0 network connections for 500ms

No new dependencies required.

### 2. networkIdle is the Right Default

| Strategy | Pros | Cons |
|----------|------|------|
| `DOMContentLoaded` | Fast | Misses SPA content |
| `load` | Gets images | Still misses XHR data |
| `networkIdle` | Gets all dynamic content | 500ms+ slower |

The 500ms cost is acceptable because:
- Static sites are already fast; 500ms is noise
- SPA content is what users actually want
- The existing 30s timeout protects against hangs

### 3. Fallback to Timeout

Some sites never reach `networkIdle` due to:
- Analytics pings
- WebSocket connections
- Polling/heartbeats

Implementation will:
1. Wait for `networkIdle` event
2. Fall back to timeout (default 30s, configurable)
3. Extract content either way

## Implementation

```rust
// browser.rs - modified goto()
pub async fn goto(&self, url: &str, timeout_ms: u64) -> Result<PageResult> {
    // 1. Subscribe to lifecycle events BEFORE navigation
    let mut lifecycle = self.page.event_listener::<EventLifecycleEvent>().await?;

    // 2. Start navigation
    self.page.goto(url).await?;

    // 3. Wait for networkIdle OR timeout
    let wait_result = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        async {
            while let Some(event) = lifecycle.next().await {
                if event.name == "networkIdle" {
                    return Ok(());
                }
            }
            Err(anyhow!("Lifecycle stream ended"))
        }
    ).await;

    // 4. Continue regardless (timeout is acceptable)
    match wait_result {
        Ok(Ok(())) => { /* networkIdle reached */ }
        Ok(Err(_)) => { /* stream ended, continue anyway */ }
        Err(_) => { /* timeout, continue anyway */ }
    }

    // 5. Extract content
    // ...
}
```

## Consequences

### Positive

- SPAs work out of the box
- No new flags or configuration needed
- Better content extraction for 90%+ of modern sites
- Zero new dependencies

### Negative

- All fetches are ~500ms slower minimum
- Sites with persistent connections may always timeout
- Slight increase in code complexity

### Neutral

- Timeout behavior unchanged (still 30s default)
- Error handling unchanged
- Output format unchanged

## Alternatives Considered

1. **Add `--wait-for networkidle` flag**
   - Rejected: Adds friction, users expect it to "just work"

2. **Fixed delay after load (e.g., 1s)**
   - Rejected: Either too short (misses content) or too long (wastes time)
   - `networkIdle` is adaptive

3. **Use playwright-rs instead of chromiumoxide**
   - Rejected: Would require rewriting browser layer
   - chromiumoxide already has lifecycle events

## References

- [Chrome DevTools Protocol - Page.lifecycleEvent](https://chromedevtools.github.io/devtools-protocol/tot/Page/#event-lifecycleEvent)
- [chromiumoxide EventLifecycleEvent](https://docs.rs/chromiumoxide/latest/chromiumoxide/cdp/browser_protocol/page/struct.EventLifecycleEvent.html)

---

*This decision improves default behavior for modern web applications without breaking existing usage.*

— Claude Opus 4.5, Principal Autonomous AI
