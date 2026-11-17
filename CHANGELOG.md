# Changelog

All notable changes to Möllendorff Ref.

## [Unreleased]

### Added

- **`user_agent` on the MCP tools `ref_fetch` and `ref_check_links`**: the same as `--user-agent`, for that call only.
  Calls sharing the server's Chrome each send their own; a call without one sends Chrome's.
  The user agent now belongs to each tab, not to the browser, and `--user-agent` uses the same path; an invalid one is refused before Chrome starts.
- **`--profile <folder|fresh>`** on `ref fetch`, `ref check-links`, `ref verify-refs`, `ref refresh-data` and `ref mcp`: chooses the Chrome profile for that run.
  Without it, `REF_CHROME_PROFILE` gives the value, as before.

### Changed

- Library: `BrowserPool::new` takes the profile value, and `CheckLinksConfig` and `RefreshConfig` have a `profile` field; the library no longer reads `REF_CHROME_PROFILE` itself (the command line does).
  The `run_*` command functions write their JSON to a writer they are given.
  New `check_links_with` and `refresh_data_with` run in a browser the caller already has (`Arc<BrowserPool>`, with plain values).
  `BrowserPool::stop` stops Chrome through a shared reference and cannot fail, so it returns nothing (`close` too); `fetch_one`, `check_links_with`, `refresh_data_with` and `BrowserPage::current_url` return `Result`, whose error is `ChromeFailed` when Chrome itself failed.
- `ref check-links`, `ref verify-refs` and `ref refresh-data` check each URL on a task of its own, at most `--concurrency` / `--parallel` at once; a panic on one URL becomes that URL's error ("Internal error in ref") instead of ending the run.
- Pages run with Chrome's cache off, so a saved profile or the MCP server's long-lived browser never answers from an old copy.
- **What ref does when Chrome crashes or hangs** is now one design, [ADR-005](docs/adr/ADR-005-CHROME-LIFECYCLE.md):
  - A Chrome failure ends the command (exit code 2, no report; a tool error under MCP) and is never reported as a fact about a URL.
    When a page fails, ref first asks whether Chrome still answers (at most 5 seconds); only a Chrome that answers makes it the page's failure, as before.
  - ref signals only the Chrome it started, never a process named in a profile's lock file.
  - Stopping asks Chrome to close and gives it 30 seconds to save a saved profile; only a Chrome that does not answer, or does not exit, is killed, with a warning.
  - Every request to Chrome has a limit, also while Chrome is frozen: 30 seconds to open and set up a tab, and 30 seconds for everything read from a page after navigation.
  - A crashed Chrome is noticed and collected at once, so it never seems to hold the profile's lock.
  - A tab whose set-up fails is closed before the error is returned, and a run that stops early cancels its remaining checks.
  - `ref mcp` stops its Chrome the same way when the server ends because its input closes, so a saved profile keeps the sign-ins of the session's last seconds.
    A server ended by a signal (Ctrl-C, SIGTERM) still kills its Chrome at once.
  - A Chrome that starts but does not answer leaves no temporary profile behind, and the error names the profile.
  - So does a Chrome that cannot start at all, also after ref retries without the sandbox: the error begins "Chrome did not start (profile ...)".
    A Chrome that exits before its first answer is an error, not a browser with an empty user agent.
- `ref check-links` and `ref refresh-data` no longer end the program from inside (`process::exit`): a file without URLs still exits with code 1, and missing arguments still exit with code 2, now with the usage text after `Error:`.
- The command line lives in the library (`ref::cli`), and `main` only calls it, so every exit code is tested in-process.
- `ref fetch`, `ref verify-refs` and `ref check-links` share one cross-host redirect check (`www.` does not count); `ref verify-refs` also treats a page whose text cannot be read as having neither a paywall nor a login.
- `ref refresh-data`, `ref fetch` and the PDF reader report every failure of one URL as that URL's failure; a PDF library panic or error is reported as "PDF extraction failed".
- A PDF locked with a real user password is an error that names the password ("PDF extraction failed: PDF error: decryption error: the supplied password is incorrect"), not "PDF contains no extractable text". The PDF library opens a file locked with the empty password while loading it; for any other lock, `ref pdf` tries the empty password so the error comes back.
- `ref check-links` reads URLs from the standard input it is given, so `--stdin` is tested with real input; its redirect check is one small function.
- All tests are inline and none is skipped: the tests that need Chrome run in plain `cargo test` (ADR-004), and CI's separate job for them is gone.
- The clean-up at exit removes only the temporary profiles of browsers that are gone, never one still in use.
- `--profile` folders are made absolute; the everyday-Chrome-profile check also sees through relative paths and symbolic links.
- `ref update` removes its download folder however the update ends; before, a failed download, checksum entry, extraction or install left it in the temporary folder.
- 100% line coverage of all code (ADR-004): CI's coverage gate and `make coverage` no longer leave any file out, and the README badge is a plain coverage badge.
- `ref login --profile fresh` is refused, since a fresh profile's sign-ins would be deleted; before, it signed in to a folder named `fresh`.
  `REF_CHROME_PROFILE=fresh` is still ignored by `ref login`, which then uses the default folder.
- An empty `REF_CHROME_PROFILE` still counts as not set.

### Fixed

- The saved-profile folder, and `.ref` above it, are created readable by their owner only (mode 0700) on macOS and Linux, not with the default permissions.
  A folder that already exists keeps its permissions.
- **Signed-in PDFs with the saved profile**: `ref fetch` (and `ref_fetch`) of a PDF behind a sign-in came back as `dead` (HTTP 403): Chrome had the session, but ref downloaded the file without it, sending only a `--cookies` file.
  ref now downloads a PDF with the cookies Chrome has for that address, the saved profile's sessions included.
- The README said `ref pdf` sees signed-in pages through the saved profile; it does not use the profile (it works without Chrome), and the README now says to use `ref fetch` for a PDF behind a sign-in.
  The README also says that `localhost` and private addresses are opened and that signed-in browsing is subject to a site's terms of service.
- One description everywhere in the repository: "Renders web pages and PDFs into compact JSON for LLM agents" (`--help`, the MCP server instructions, the crate docs and `.asimov/project.yaml`).
- A page that only mentions `isAccessibleForFree: false` is no longer a `paywall`: the signal counts only in the page's JSON-LD or microdata, not in its text or other scripts.
  ref's own GitHub page, whose README explains the signal, came back as a paywall.
- A `main` element that holds exactly one `article` with at least a third of its text gives just that article as content, for `ref fetch` and `ref refresh-data`.
  A GitHub repository page now starts with its README instead of the file list and the About, Stars and Forks sidebar.
- **MCP with a saved profile**: `ref_check_links`, `ref_refresh_data` and `ref_verify_refs` failed once `ref_fetch` had run, because each started a second Chrome on the profile the server's browser already held.
  They now share the server's browser.
- The "profile is open in another Chrome" error names the possible holders (`ref login`, a running `ref mcp`, another ref command) instead of only `ref login`.
- **MCP after a Chrome crash or hang**: the call that meets the failure gets a tool error saying so, and the next call starts a new Chrome, instead of every later call failing.
- **`verify-refs` with a failing Chrome** wrote every reference as `dead`, also when Chrome died in the middle of loading a page.
  Now it stops at the first Chrome failure with an error naming the URL, and leaves `references.yaml` as it was.
  The same holds when Chrome still answers but a tab cannot be opened: that says nothing about the URL either.
- MCP calls with nothing to do (no URLs, nothing chosen, a missing file) answer without starting Chrome, so the real error is reported.
- MCP `concurrency` / `parallel` values of 17 to 20 were quietly capped at 16 tabs; the shared browser now has 20.

## [1.9.0] - 2026-11-15

Set the user agent, for sites such as SEC EDGAR.

### Added

- **`--user-agent`** on `ref fetch` and `ref check-links`: sends exactly the text you give instead of Chrome's own user agent with ref's name added, for pages and for PDF downloads. SEC EDGAR needs a name and an email in it and answers HTTP 403 otherwise. An empty or multi-line value is refused.
- A 403 from `ref fetch` now adds an alert that the site may want a declared user agent, unless `--user-agent` was already given.
- The MCP server does not take the option yet.

### Changed

- CI: the coverage cache is keyed on the Rust version, so the 100% gate holds on Rust 1.100 (#45).

## [1.8.4] - 2026-11-06

Release provenance and wording; no code changes.

### Added

- **Build-provenance attestation** for every release file (`actions/attest-build-provenance`). Verify with `gh attestation verify <file> --repo mollendorff-ai/ref`. `checksums.txt` still proves integrity only; the attestation proves which workflow built the file.

### Changed

- ADR author lines and `.asimov/sprint.json` use plain wording ("AI assistant, under the maintainer's direction"); the autonomy instructions are unchanged in meaning.

## [1.8.3] - 2026-11-06

PDF extraction fixes from the 1.8.1 evaluation.

### Fixed

- **PDF tables were rarely found.** pdf-extract writes one space for any gap, so tables whose cells are drawn at separate positions (most generated reports, LaTeX `tabular`, reportlab) came out as "Region Q3 Q4" with no columns to find. ref now writes the text itself from pdf-extract's character positions and keeps wide gaps, padded to the column where the next cell starts, so cells line up across rows.
- Tables with spaced-out rows (a blank line between rows) are no longer cut into one-line pieces; after a blank line, only a row with as many cells continues the table, and a heading line ("2.1  Notes on the data") never becomes a row.
- Right-aligned columns (numbers) no longer split words ("Regio | n"): when every row has the same number of cells, each row is split on its own gaps.
- Header rows: labels with a digit ("Q3", "FY2026") no longer count as numbers, and a first row styled like the rest (Title Case over Title Case) is no longer taken as a header.
- **A heading on the first line** ("1. Introduction") is now a section heading instead of text under "Content".
- Confident headings (numbered, Roman numerals, keywords) start a section after any text, not only after 50 characters, so a short title line or a short table no longer hides the next heading. Weak guesses (ALL CAPS, mostly uppercase) keep the 50-character rule. A line with two or more column gaps is never a heading.
- **Author**: "by region, in thousands of dollars" inside a sentence was reported as the author. Only a labelled line ("Author: …", "Written by …") or a line that is just "By" and names counts.

### Added

- PDF title and author come from the PDF's metadata (Info dictionary) when usable; file names left by the producing program ("report_v3.docx"), "Untitled" and placeholder authors ("anonymous") are ignored.
- `tests/fixtures/report.pdf` (with the script that makes it): a real PDF with a first-line heading, a table drawn cell by cell with right-aligned numbers and spaced-out rows, and a heading drawn as number and title. Unit tests check its sections, table, title and author.

### Changed

- Section text in PDFs uses single spaces; the column layout is kept in the `tables` array.
- `src/pdf.rs` (about 2,000 lines) is split into `src/pdf/` modules: `layout` (text and metadata from the PDF), `tables`, `headings`, `sections`, `fields` (title, author, date, DOI, links, code) and `mod` (command, download, page). No behaviour change: the output of 31 PDFs, fetch and refresh-data is byte-identical before and after the split.
- Constant regexes are compiled once (`LazyLock` statics) instead of on every call, in `pdf`, `extract`, `scan` and `access`; the last `unwrap()` outside tests is gone. The unit tests run about 4× faster.
- CI and Makefile coverage exclusions name the `pdf/` folder instead of `pdf.rs`, so the same code stays outside the 100% gate.
- The author pattern uses Latin letter ranges (ASCII, Latin-1, Latin Extended-A) instead of Unicode classes, which took about 33 ms to compile on every PDF. With it, 1.8.3 extracts PDFs within 1–3% of 1.8.2's time (release build, median of 15 runs on 35-page documents).

## [1.8.2] - 2026-11-06

Small fixes from the 1.8.1 evaluation.

### Fixed

- **`refresh-data` amounts**: a currency sign with no number beside it (for example `M$.` at the end of a sentence) was reported as the amount `"."`. A number is now required, and trailing punctuation and spaces are no longer part of `raw`. Percentages follow the same rule.
- **`ref_scan` refusals**: an output that is not `.yaml` or `.yml`, or an existing file that is not a references file, now returns invalid params (`-32602`) like the other workspace refusals, instead of an internal error (`-32603`).

### Added

- `refresh-data` reads amounts written with the sign after the number (`4,5 M$`, `150 €`) and the signs `€`, `£`, `CA$`, `US$`, `C$` and `A$`, not only `$`. Each amount has a new `currency` field.

### Changed

- Makefile: the Linux ARM64 build targets `aarch64-unknown-linux-musl` like the release workflow; the Windows targets are removed (the Release workflow builds Windows with MSVC); `make lint` uses `--all-targets` and the coverage exclusions match CI.
- CHANGELOG: the dates of 0.6.0 to 1.1.0 are corrected to match the git history (the repository starts on 2025-12-07). 0.6.0, 0.7.0 and 0.7.1 are dated by month only because no commit marks them.

### Documentation

- README: the `Content` heading for text before the first heading, the character-encoding limitation, and the Chrome 144+ opt-in to remote debugging.
- ADR-001: an update note (rmcp 3.9, only `ref_fetch` shares a browser, saved profile). `.asimov/project.yaml` lists the real dependencies.

## [1.8.1] - 2026-11-06

### Added

- Runs with a saved profile print `Using saved profile <folder>` on stderr, so you can tell which profile a run used.

### Documentation

- README: step-by-step "First time" and "Later" guides for signed-in reading (what to do when a session expires, adding a site, upgrading from 1.7), where the profile lives on each system, and "Sign-ins and human checks" listed among the problems ref solves.

## [1.8.0] - 2026-11-06

### Added

- **Saved profile for signed-in sites.** `ref login [urls]` opens a visible Chrome with ref's saved profile (`~/.ref/chrome-profile`); you sign in, accept cookie banners and pass "I am human" checks yourself, and every later `fetch`, `pdf` and MCP call reuses those sessions headless. The profile is never deleted. 1.7.0 made every run start from an empty temporary profile, which lost these sessions.
- `REF_CHROME_PROFILE=<folder>` selects another saved profile; `REF_CHROME_PROFILE=fresh` forces a temporary one.
- Clear errors when the profile is open in another Chrome, and when it points at your everyday Chrome profile (Chrome 136 and later refuses remote control of it).

### Changed

- With a saved profile, the user agent is Chrome's own (sessions and human-check passes are often tied to it); temporary profiles keep the `mollendorff-ref/<version>` suffix.

## [1.7.1] - 2026-11-06

Fixes from a second independent review of 1.7.0.

### Fixed

- **`ref_scan` could overwrite any file in the workspace** (for example a `.rs` source file), and through a hard link a file outside it. Outputs must now be `.yaml` or `.yml`, an existing file must be empty or already a references file, hard-linked files are refused, and all writes (`scan`, `verify-refs`, `init`) go to a temporary file that is renamed into place, so a hard link is never written through.
- **Signed-in PDFs failed**: when `fetch` found a PDF it downloaded it again without the cookies or Chrome's user agent, so a members-only PDF came back `dead` with `http: 200`. The download now sends the same user agent and the cookies that apply to the URL, and `http` is reported only when the download succeeded. Timeouts say "Download timed out (limit 60 s)".
- **Ref's Chrome feature list usually lost to chromiumoxide's**: they were two separate `--disable-features` switches in random order. The list is now merged into chromiumoxide's own switch, so Chrome receives one deterministic list (checked in the launched command line).
- **Shadow DOM text was dropped silently**: text in open shadow roots (web components) is now copied into the page before it is read, nested components included. Closed shadow roots still cannot be read.
- **Hidden overlays counted as paywalls**: wall elements hidden with `hidden`, `aria-hidden="true"`, `display: none` or `visibility: hidden` are ignored.
- **SIGKILL left Chrome running**: on Linux, the next run stops Chrome processes left by a killed run and removes their profile.
- `ref mcp --root` with a missing folder names the folder in the error.

### Documentation

- README: the output example's `chars` matches its content; "the browser stays warm" now says only `ref_fetch` reuses the browser; `ref_scan` output rules and SIGKILL cleanup are described.
- `.asimov/freshness.json` no longer forbids other web tools; it says to prefer ref for reading pages and to check `status`, `http` and `truncated`.

## [1.7.0] - 2026-11-06

Correctness, safety and honesty release: real HTTP status, no silently dropped or cut content, http(s)-only fetching, a contained MCP server, verified self-update, and a README that matches the code.

### Fixed (from an independent review after the first round)

- **Chrome flags were never applied.** chromiumoxide adds the leading `--` itself, so every extra switch ref passed (including `--headless=new` and the background-networking switches) reached Chrome as `----flag` and was ignored. Flags are now passed correctly. The README no longer claims Chrome's calls to Google are disabled: the switches reduce them, but Chrome still makes some (component update, account checks).
- **MCP tab leak**: tabs were never closed, so a long-running server grew by one Chrome target per call. A tab now closes when its page is done.
- **MCP parallelism was fixed by the first call**: the shared browser now allows 16 tabs and each call is limited by its own `parallel`. Four 3-second pages take about 3 s, not 12 s.
- **Workspace guard escapes**: files matched by a `ref_scan` glob through a symlinked folder, and outputs that are symlinks to missing files, are now refused. `ref mcp --root <folder>` sets the workspace explicitly; file tools are refused when the workspace is the filesystem root.
- **Hangs**: `parallel` / `concurrency` of 0 (or above 20) is rejected in MCP and the CLI instead of hanging. PDF downloads have a 15 s connect and 60 s total timeout, and the 100 MB cap is checked while streaming (and against `Content-Length`), not after downloading everything. PDFs are parsed from memory, with no temporary file.
- **`refresh-data` reported success on 404s and refused connections**; navigation errors and 4xx/5xx are now failures with the HTTP code, and the command exits 1 when any URL failed. Numbers come from the page's main content, so ads no longer leak in.
- **Silent empty results**: a page wrapped in `<form>` (ASP.NET style) lost all its content; forms are no longer dropped. New alerts for pages with no readable content, a `--selector` that matched nothing or is invalid, and the 50-link cap. A URL that turns out to be a PDF is extracted as `ref pdf` would.
- **Paywall detection** matched class substrings, so `class="no-paywall"` was flagged; markers now match whole name parts and negated names are ignored.
- **Ctrl-C and SIGTERM** close Chrome and remove its temporary profile (exit code 130). Leftover profiles from killed Chrome processes are also removed on normal exit.
- **Character counts**: `chars` and the section limit count characters, not bytes (French text was cut early).
- `ref pdf` on a relative path reports a proper absolute `file://` URL.
- `ref scan --merge` could not be turned off; use `--no-merge` to overwrite (`--merge` is still accepted).

### Added

- `--max-sections <N>` for `fetch` (and `max_sections` for MCP `ref_fetch`), default 50, `0` for no limit; the truncation alert names every limit that applied.

### Removed

- The Instagram-follower and Statista extractors in `refresh-data`; it now has one generic extractor for amounts and percentages. The JSON fields `type`, `followers` and `username` are gone; `http` was added.
- The e2e test that needed example.com (the local-server suite covers check-links), and the stale `RUSTSEC-2025-0057` ignore (fxhash is no longer a dependency).

### Fixed

- **Real HTTP status**: `fetch`, `check-links` and `verify-refs` now read the main document's HTTP status from Chrome instead of guessing from the page title. A 404 page was reported as 200, and a page titled "Fortune 500" as 500.
- `fetch` marks any 4xx or 5xx response as `dead`, adds an `HTTP <code>` alert, and reports the code in a new `http` field.
- `verify-refs` treats every 4xx as dead (it only caught 404 and 5xx).
- Paywall and login detection no longer fire on ordinary articles that mention those words. A page is flagged only for structured data (`isAccessibleForFree: false`), a wall overlay element, or wall wording or a password field on a page with little readable text. `fetch` and `verify-refs` now share one detector.
- `check-links` no longer panics on long non-ASCII URLs (byte slicing inside a character).
- **No more silently dropped content**: `fetch` keeps lists (as Markdown bullets), tables (as Markdown tables), block quotes, definition lists, figure captions and loose text in `div`s, in document order. Before, only `<p>` and headings survived, so tables and lists vanished.
- Text before the first heading is kept in a `Content` section (it was dropped).
- Navigation, headers, footers, asides and forms are now actually stripped outside the article (the strip step was a no-op). `--raw` keeps them.
- PDF tables without detected headers now produce valid Markdown (an empty header row plus the `---` separator).
- MCP `ref_fetch` fetches its URLs concurrently (it ran them one at a time).
- **`--cookies` works**: the Netscape cookie file is parsed and sent with each page. Before, the option was accepted and ignored.
- Each run uses its own Chrome profile folder, removed on exit, so two ref processes no longer collide on one shared profile.
- The user agent is Chrome's own (with "HeadlessChrome" shown as "Chrome") plus `mollendorff-ref/<version>`, instead of a hard-coded 2023 Mac Chrome 120 string.
- Fixed two `clippy::needless_raw_string_hashes` errors that failed the Lint job on current stable.

### Tests

- New end-to-end suite (`tests/e2e_local.rs`, runs in the Chrome CI job) against a local HTTP server: 404 and 500 are dead with exit 1, a "Fortune 500" title is ok, same-host redirects are followed, lists/tables/quotes/loose text are kept and nav/footer dropped, long pages are flagged or kept whole with `--max-chars 0`, content loaded by `fetch()` after page load is seen, wall detection on real and false cases, `file://` refused, and check-links reports real status codes. Each case was a bug found in review.

### Documentation

- README rewritten to match what the code does: no "bypasses bot protection" claim (also removed from the CLI help, crate docs and MCP server instructions), a status note that says it is a young personal project built with AI assistance, documented exit codes, status values, truncation, security defaults and a Limitations section. The Asimov section no longer recommends forbidding other web tools.
- `docs/mcp-integration.md` documents `max_chars`, concurrent fetching and the working-directory rule for paths.
- Removed the `deploy-kveldulf` Makefile target (a personal machine).

### Maintenance

- **rmcp 0.16 → 3.9** (MCP SDK; clears RUSTSEC-2026-0189, which affected only its HTTP transport) and **scraper 0.25 → 0.27**. Supersedes Dependabot PRs #20 and #21.
- Removed unused dependencies: `html2text`, `thiserror`, and the direct `schemars` (rmcp re-exports it).
- The `ref` binary now uses the library instead of compiling every module a second time, so unit tests run once and the test badge no longer counts them twice.
- `Cargo.lock` is committed, so CI and release builds use the same dependency versions.
- CI: the Lint job is pinned to Rust 1.99.1, so a new Rust release cannot turn main red without a code change; a `cargo audit` job runs on every change and weekly; the release workflow now runs fmt and pedantic clippy before building (v1.6.0 was released with Lint failing).
- The coverage badge is labelled "logic coverage": it measures the pure-logic modules only (I/O modules are covered by the Chrome end-to-end tests).

### Security

- **Only `http` and `https` URLs** are opened by `fetch`, `check-links`, `verify-refs`, `refresh-data` and PDF downloads. `ref fetch file:///etc/hostname` used to return the file, also through MCP.
- **MCP file access is limited to the server's working directory**: `ref_pdf`, `ref_scan` (inputs and output), `ref_verify_refs` and `ref_refresh_data` refuse absolute paths, `..` and symlinks that lead outside it. `ref_scan` could write anywhere.
- **Chrome sandbox on by default**. It is turned off only when running as root, when `REF_CHROME_NO_SANDBOX=1` is set, or when Chrome cannot start with it (ref retries once and prints a warning).
- **`ref update` verifies the SHA-256** of the download against the release's `checksums.txt` and refuses to install on a mismatch or when the checksum is missing.
- **PDF parsing upgraded**: `pdf-extract` 0.10 → 0.12, which moves `lopdf` 0.38 → 0.42 (RUSTSEC-2026-0187, stack overflow on deeply nested objects). A panic inside the PDF parser now becomes a `dead` result instead of crashing the process or the MCP server (release builds unwind instead of abort). PDF downloads are capped at 100 MB.
- Chrome's background traffic to Google (updates, sync) is disabled.

### Added

- **`--max-chars <N>`** for `fetch` (and `max_chars` for MCP `ref_fetch`): per-section limit, default 5000, `0` for no limit.
- **Truncation is reported**: a cut section has `"truncated": true`, the page has `"truncated": true` and an alert saying how to get the full text. PDFs report their 10,000-character section limit the same way. Before, long content was cut to 2,000 characters per paragraph with no signal.

### Changed

- **Exit codes**: 0 when everything succeeded, 1 when any URL or file failed, 2 when the command itself failed. Previously failures exited 0.
- Navigation errors keep Chrome's message after the short label (for example `DNS_FAILED: net::ERR_NAME_NOT_RESOLVED`).

## [1.6.0] - 2026-03-07

PDF from URL and targeted extraction with CSS selector.

### Added

- **`ref pdf <url>`**: accepts https:// URLs in addition to local file paths — downloads the PDF and extracts in one step, no manual curl required
- **`ref fetch --selector <css>`**: skip content heuristics and extract a specific DOM element directly. Falls back to normal content extraction if the selector matches nothing
- MCP tools updated: `ref_pdf` accepts URL input; `ref_fetch` exposes `selector` parameter

## [1.5.1] - 2026-03-07

Bug fixes for section extraction and link-check concurrency.

### Fixed

- `extract_sections()` now walks the DOM in document order using a single CSS selector pass. Previously the multi-pass loop over heading tags caused paragraphs to be mis-assigned — all paragraphs were appended to whichever heading was last encountered across all tag-specific loops, losing document structure.
- `check_links()` now runs URL checks in parallel using the BrowserPool concurrency pool. Previously the `--concurrency` flag had no effect — checks ran sequentially despite the pool being created with the requested concurrency.

## [1.5.0] - 2026-02-22

MCP Server Mode — persistent JSON-RPC 2.0 server for AI tool integration.

### Added

- **`ref mcp`** subcommand: starts MCP server over stdio with persistent browser pool
- **6 MCP tools**: `ref_fetch`, `ref_pdf`, `ref_check_links`, `ref_scan`, `ref_verify_refs`, `ref_refresh_data`
- **MCP integration guide**: `docs/mcp-integration.md` with setup for Claude Code, Claude Desktop, VS Code
- **ADR-001 updated**: Status changed from Deferred to Accepted

### Changed

- Core functions (`fetch_one`, `extract_pdf`, `scan_files`, `verify_refs_core`) promoted to `pub(crate)` for MCP reuse
- `scan.rs` and `verify_refs.rs` refactored: core logic extracted from CLI print wrappers

### CI

- **Coverage gate**: `cargo-llvm-cov` enforces 100% on pure-logic modules, gates auto-release
- **Dynamic badges**: test count and coverage percentage updated on every push to main via GitHub Gist
- **actions/checkout** 4 → 6, **actions/cache** 4 → 5

### Dependencies

- **rmcp** 0.16 (Anthropic official MCP SDK)
- **schemars** 0.8 (JSON Schema generation for tool parameters)
- **tracing** 0.1 + **tracing-subscriber** 0.3 (structured logging to stderr)

## [1.4.0] - 2026-02-22

Dependency hygiene and automation.

### Added

- **Dependabot**: Weekly automated PRs for cargo and github-actions dependencies
- **cargo audit**: Zero known vulnerabilities confirmed

### Changed

- **regex** 1.11 -> 1.12
- **reqwest** 0.13.1 -> 0.13.2
- **assert_cmd** 2.0 -> 2.1
- **tempfile** 3.14 -> 3.25

## [1.3.1] - 2026-02-21

Dependency upgrades and quality gate fixes.

### Changed

- **chromiumoxide** 0.7 -> 0.9 (tokio runtime now built-in)
- **scraper** 0.22 -> 0.25
- **html2text** 0.14 -> 0.16
- **pdf-extract** 0.8 -> 0.10
- **thiserror** 1.0 -> 2.0
- 72 transitive dependencies updated via `cargo update`

### Fixed

- Pre-commit coverage gate now excludes IO modules per ADR-003 (enforces 100% on pure-logic modules only)
- Markdown lint config added (`.markdownlint.jsonc`) — disables rules incompatible with project style
- Blank-line violations in CHANGELOG and ADRs

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
- Windows build: `#[cfg(unix)]` guard on `PermissionsExt`, `.zip` archive detection for Windows assets
- CI clippy enforces `--all-targets` (lints tests too, not just library)
- Migrated from deprecated `cargo_bin()` to `cargo_bin_cmd!()` macro in integration tests
- Chrome-dependent test moved behind `#[ignore]` per ADR-003

### Added

- ADR-003: Test strategy — unit tests for pure logic, `#[ignore]` e2e tests for Chrome-dependent code
- **GitHub Actions**: CI workflow with clippy pedantic, unit tests (stable + beta), e2e tests (Chrome), release build
- **GitHub Actions**: Auto-release on version bump (tag + multi-platform binaries + crates.io publish)
- **GitHub Actions**: E2E test job — runs `#[ignore]` tests with Chrome on ubuntu runners

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

## [1.1.0] - 2026-01-01

Rebrand & Release Infrastructure.

### Changed

- **Renamed from RoyalBit to Möllendorff AI**

### Added

- Cargo.toml: crates.io publishing metadata
- GitHub Actions: CI workflow (test, lint, build)
- GitHub Actions: Release workflow (multi-arch)
- Targets: linux-x64, linux-arm64, macos-x64, macos-arm64, windows-x64
- `update` command: self-update from GitHub releases
- Updated LICENSE, README, schemas for new branding

## [1.0.0] - 2025-12-15

Full LLM reference toolkit.

### Added

- `pdf` command: extract text from PDF files
- `pdf` command: output same JSON structure as fetch

## [0.9.0] - 2025-12-15

### Added

- `init` command: create references.yaml template
- `scan` command: extract URLs from markdown files
- `scan` command: dedupe, track cited_in per reference
- `scan` command: infer categories from file paths

## [0.8.0] - 2025-12-15

### Added

- deploy-kveldulf target (remote build)
- Simplified Makefile

## [0.7.1] - 2025-12

### Added

- `check-links` command: compact JSON output
- `refresh-data` command: compact JSON output

## [0.7.0] - 2025-12

### Added

- `fetch` command: structured sections[], links[], code[] output
- Content cleaning (strip nav/footer/aside)
- `--raw` and `--cookies` flags

## [0.6.0] - 2025-12

### Added

- `verify-refs` command with references.yaml schema
- Status detection (ok/dead/redirect/paywall/login)
- JSON Schema v1 (schemas/references.v1.schema.json)
