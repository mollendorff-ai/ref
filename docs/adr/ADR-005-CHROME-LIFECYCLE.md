# ADR-005: Chrome Lifecycle

**Status:** Proposed
**Date:** 2026-11-17
**Author:** Claude (AI assistant, under the maintainer's direction)

---

## Context

ref starts a Chrome, sends it requests, and closes it.
Until 1.9.0 nothing said what happens when that Chrome crashes or hangs, and the answer was "whatever each call site does".

On 2026-11-17 the MCP tools were changed to share one long-lived Chrome, which made the question urgent.
Three rounds of fixes followed, each reviewed, and each review found new problems in the same code:

- `verify-refs` wrote every reference as `dead` when Chrome died.
- A dead Chrome made requests wait instead of fail; a frozen one made them wait forever.
- To replace a dead Chrome, the MCP server killed the process named in the profile's lock file, which can be a `ref login` window or an unrelated process.
- Closing killed any Chrome that took more than 5 seconds to exit, including a healthy one still saving sign-ins.
- A fix for exited-but-uncollected processes read `/proc`, so it worked on Linux only.

The fixes were local answers to questions that need one answer: which process is ours, when is it dead, when may we kill it, and what may be recorded about a URL when it fails.

## Decision

Seven rules.
Each has one place in the code and at least one test.

### 1. A pool owns the Chrome it started, and only that one

A pool records the process id of its Chrome when it starts it, from the child process handle.
ref sends a signal only to that process, and only while the pool has not been stopped.
It never signals a process named in a lock file.

The number cannot be reused while the pool holds it: a child process keeps its number until its parent collects it, and the pool collects it only in rule 4.

The existing clean-up of profiles left by killed ref runs (`sweep_orphans`) is unchanged: it matches Chrome processes by a temporary profile folder named for a ref process that no longer exists.

### 2. One question decides dead or slow

`is_alive` asks Chrome for its version and waits at most **5 seconds**.
An answer means alive.
No answer, or an error, means the Chrome has failed.

A Chrome that crashed or was killed fails this at once, because ref stops listening to a connection that has failed.
A frozen Chrome fails it after 5 seconds.
A busy Chrome answers: the version comes from the browser process, not from a page.

### 3. A Chrome failure fails the command, and is never a fact about a URL

When a request to Chrome fails or runs into its time limit, ref asks rule 2's question before deciding what the failure means.

- **Alive**: the failure belongs to the page (site down, page too slow, tab crashed), and is reported for that URL as today.
- **Not alive**: the failure is Chrome's.
  It travels as one typed error, `ChromeFailed`, and the pool is marked failed.

A failed pool stays failed: every later request on it fails at once with `ChromeFailed`, without waiting.

`ChromeFailed` ends the run:

| Where | What happens |
| --- | --- |
| `ref verify-refs` | `references.yaml` is left as it was; the error names the URL; exit code 2 |
| `ref fetch`, `check-links`, `refresh-data` | no report is printed; the error is on stderr; exit code 2 |
| MCP tools | the call returns a tool error saying Chrome failed and will be started again on the next call |

This is the README's existing meaning of exit code 2, "the command itself failed".
Results gathered before the failure are discarded: an agent that sees `status: dead` treats it as a dead link, so a report that mixes real results with Chrome failures would mislead.

### 4. Stopping: ask, wait, then kill

A pool is stopped through `&self`, so it does not matter who else still holds it.
The browser handle sits behind a read-write lock: requests share it, stopping takes it alone.

**Graceful stop** (the end of every command):

1. Ask Chrome to close, and wait up to 5 seconds for it to acknowledge.
2. If it acknowledged, wait up to **30 seconds** for it to exit: it may be saving cookies and sessions of a saved profile.
3. If it did not acknowledge, or did not exit, kill it (rule 1) and say so on stderr; with a saved profile, also say that sign-ins from this run may not have been saved.

**Forced stop** (a pool that has failed): kill it at once, since a Chrome that does not answer is not saving anything.

Both end by collecting the process, with a 5-second limit.
Dropping a pool that was never stopped still kills its Chrome, as today; that is the backstop, not the normal path.
After that the pool is stopped: it signals nothing, and the process is gone on every platform, so the profile's lock check needs no special case.

After a kill, a fresh profile folder is removed, waited on and removed again, as the orphan clean-up already does, and stays on the list for the final clean-up until it is gone.

On Unix the kill is a signal to the recorded process id, so it does not wait for requests in flight; they fail as the connection closes.
Elsewhere the kill goes through the browser handle and waits for requests in flight, at most the request limit.

### 5. The MCP server replaces a failed Chrome between calls

Before a call uses the shared pool, the server asks rule 2's question while holding the pool slot.
If the pool has failed, the server stops it (forced, rule 4) and starts a new one.
Calls still running on the old pool end with `ChromeFailed` (rule 3).

Other calls wait behind the slot during this: at most 5 seconds for the question, the forced stop, and Chrome's start.

A Chrome that is alive but slow to answer for more than 5 seconds is replaced too.
That is deliberate: the alternative is an agent waiting on a browser nobody is watching.

### 6. One table of time limits

| What | Limit |
| --- | --- |
| Is Chrome alive (rule 2) | 5 s |
| Starting Chrome, up to its first answer | 30 s |
| Opening a tab and setting it up (user agent, cookies, cache off, events), together | 30 s |
| Navigation | `--timeout` (as today) |
| Everything read from a page after navigation (title, address, web components, HTML), together | 30 s |
| Chrome acknowledging a close | 5 s |
| Chrome exiting after it acknowledged | 30 s |
| Collecting a killed Chrome | 5 s |

Every request to Chrome goes through one helper that applies its limit.
chromiumoxide's own request timeout stays at its default but is not relied on: it does not fire while Chrome is frozen (seen on 2026-11-17 with `SIGSTOP`).

A tab is closed whichever way its set-up ends, the limit included.

### 7. A run that stops early stops its work

`task::isolated` cancels its task when the caller stops waiting for it.
A run that ends on the first `ChromeFailed` therefore leaves no checks running in the background.

## What this removes

- `BrowserPool::stop_chrome`, which killed by lock file.
- `is_zombie` and the `/proc/<pid>/stat` reading in `process_alive`: after rule 4 the pool's own Chrome is collected, not left as a zombie.
- `BrowserPool::close_shared`: stopping no longer depends on being the last holder.
- The unconditional kill 5 seconds after asking Chrome to close.

## Tests

Each rule is tested with a real Chrome, frozen with `SIGSTOP` or killed with `SIGKILL` where the rule is about failure:

1. A pool whose Chrome was killed, on a saved profile that a second Chrome has since opened: stopping the pool leaves the second Chrome running.
2. `is_alive`: true for a healthy Chrome, false at once for a killed one, false within about 5 seconds for a frozen one.
3. Chrome killed or frozen during navigation: `verify-refs` leaves the file byte for byte, and `fetch`, `check-links` and `refresh-data` return `ChromeFailed`; a site that refuses the connection is still that URL's failure; a failed pool fails its next request at once.
4. Graceful stop of a healthy Chrome on a saved profile keeps a cookie set in that run; a frozen Chrome is killed and its fresh profile folder is gone; stopping twice is harmless; stopping works while another holder still has the pool.
5. MCP: after its Chrome is killed, and after it is frozen, the next call succeeds on a saved profile; a call running during the failure returns the tool error.
6. A page whose reads hang is given up after the read limit, not three times the request limit; a tab whose set-up fails or times out is closed.
7. After a run stops early, no task of it is still running.

## Platforms

Linux is what CI tests.

The design uses nothing Linux-only for a pool's own Chrome, so it is expected to hold on macOS, but that is untested here.
The manual macOS check in the roadmap gains two steps: kill ref's Chrome while `ref mcp` runs and make another call, and run `ref login` after such a crash and check that the window stays open.

On Windows, builds are produced and nothing is tested; a forced stop there waits for requests in flight (rule 4).

## Alternatives considered

- **Start Chrome ourselves and connect to it**, so ref holds the child process directly instead of reaching it through chromiumoxide.
  Cleaner ownership, but ref would have to rebuild chromiumoxide's launch (its default flags, finding the debugging address), and a differing flag changes how pages render.
  The read-write lock gives the same control with a smaller change.
- **No automatic restart**: after a failure the MCP server says "restart the server".
  Smallest change, but an agent cannot restart its own MCP server, so one crash would end its web access for the session.
- **Keep partial results** when Chrome fails mid-run.
  Needs a new status value in the output so that a Chrome failure cannot be read as a dead link; a change to the output format is its own decision.

## Consequences

### Positive

- One answer each to: which process, dead or slow, when to kill, what may be recorded.
- `verify-refs` cannot write a Chrome failure into `references.yaml`, whenever Chrome fails.
- ref cannot kill a process it did not start (the orphan clean-up aside, which is unchanged).
- A healthy Chrome gets 30 seconds to save a saved profile before it is killed.

### Negative

- A Chrome failure late in a long run discards that run's results (rule 3).
- A failed navigation costs up to 5 extra seconds when Chrome is frozen (rule 2's question).
- A Chrome that is alive but unresponsive for 5 seconds is killed under MCP (rule 5), and sign-ins saved in its last seconds may be lost.
- Requests take a shared lock on the browser handle; stopping waits for it on non-Unix systems.
