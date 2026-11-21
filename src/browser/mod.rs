//! Headless Chrome browser management via chromiumoxide
//!
//! The pool and the life of its Chrome (ADR-005): which process is ours, how
//! dead is told from slow, how Chrome is stopped. Tabs are in [`page`], starting
//! Chrome and its processes and profile folders in [`process`].
//!
//! v1.2.0: Added `networkIdle` wait for SPA support (ADR-002)

use crate::cookies::Cookie;
use crate::profile::{self, Profile, ProfileChoice};
use anyhow::{Context, Result};
use chromiumoxide::cdp::browser_protocol::network::{
    CookieParam, SetCacheDisabledParams, SetCookiesParams, SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::page::SetLifecycleEventsEnabledParams;
use chromiumoxide::{Browser, Page};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

mod page;
mod process;

pub use page::{BrowserPage, PageResult};
pub(crate) use process::{
    detect_chrome_path, prepare_saved_profile, running_as_root, want_sandbox,
};
use process::{
    honest_user_agent, kill_process, launch, remove_dir_retrying, sweep_orphans, to_cookie_param,
    track_profile, unique_profile_dir, untrack_profile,
};
pub use process::{remove_open_profiles, validate_user_agent};

// Time limits (ADR-005 rule 6). Navigation has its own: `--timeout`.

/// Is Chrome alive (rule 2)
const ALIVE_LIMIT: Duration = Duration::from_secs(5);
/// Starting Chrome, up to its first answer; also opening a tab and setting it
/// up, together; and everything read from a page after navigation, together
const CHROME_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Chrome acknowledging a close
const CLOSE_ACK_LIMIT: Duration = Duration::from_secs(5);
/// Chrome exiting after it acknowledged a close (it may be saving a saved profile)
const EXIT_LIMIT: Duration = Duration::from_secs(30);
/// Collecting a killed Chrome
const COLLECT_LIMIT: Duration = Duration::from_secs(5);

/// Chrome itself failed: it crashed, was killed, or stopped answering (ADR-005
/// rule 3). Never a fact about a URL: it ends the command.
#[derive(Debug)]
pub struct ChromeFailed(pub String);

impl std::fmt::Display for ChromeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chrome failed: {}", self.0)
    }
}

impl std::error::Error for ChromeFailed {}

/// Split a result: a Chrome failure stays an `Err` (it ends the command), and
/// any other error becomes `Ok(Err(message))`, the page's own failure.
///
/// # Errors
/// Returns the error when it is a [`ChromeFailed`].
pub fn page_failure<T>(result: Result<T>) -> Result<std::result::Result<T, String>> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(e) if e.is::<ChromeFailed>() => Err(e),
        Err(e) => Ok(Err(e.to_string())),
    }
}

/// The Chrome a pool started, shared by the pool and its tabs
struct Chrome {
    /// Requests share it; stopping takes it alone (rule 4). In an `Arc` so a
    /// request can hold its lock on a task of its own ([`open_tab`](Self::open_tab))
    browser: Arc<tokio::sync::RwLock<Browser>>,
    /// The process this pool started, until it has been collected: the only
    /// process ref may signal (rule 1)
    pid: tokio::sync::Mutex<Option<u32>>,
    /// Set once Chrome failed; every later request fails at once (rule 3)
    failed: std::sync::atomic::AtomicBool,
    stopped: std::sync::atomic::AtomicBool,
    request_timeout: Duration,
}

impl Chrome {
    fn has_failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::SeqCst)
            || self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn mark_failed(&self) {
        self.failed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn failure(what: &str) -> anyhow::Error {
        anyhow::Error::new(ChromeFailed(format!("it no longer answers ({what})")))
    }

    /// Rule 2: one question, at most [`ALIVE_LIMIT`]. A failed answer marks
    /// the pool failed.
    async fn alive(&self) -> bool {
        if self.has_failed() {
            return false;
        }
        let browser = self.browser.read().await;
        let answered = tokio::time::timeout(ALIVE_LIMIT, browser.version())
            .await
            .is_ok_and(|v| v.is_ok());
        if !answered {
            self.mark_failed();
        }
        answered
    }

    /// Every request to Chrome goes through here (rules 3 and 6), with a
    /// deadline: chromiumoxide's own request timeout does not fire while
    /// Chrome is frozen. A failed request is Chrome's failure when Chrome no
    /// longer answers, else the page's.
    async fn ask<T, E: Into<anyhow::Error>>(
        &self,
        until: tokio::time::Instant,
        what: &str,
        request: impl std::future::Future<Output = std::result::Result<T, E>>,
    ) -> Result<T> {
        // Generic: every line runs for every answer type, so that each copy
        // the compiler makes is covered whole; the branches are in
        // `refuse_if_failed` and `failure_behind`
        self.refuse_if_failed(what)?;
        let answer = Self::attempt(until, what, request).await;
        let failure = self.failure_behind(answer.as_ref().err(), what).await;
        failure.map_or(answer, Err)
    }

    /// A failed pool asks nothing (rule 3)
    fn refuse_if_failed(&self, what: &str) -> Result<()> {
        if self.has_failed() {
            Err(Self::failure(what))
        } else {
            Ok(())
        }
    }

    /// For a failed request (`error`), Chrome's failure when Chrome no longer
    /// answers; `None` when the request did not fail or the failure is the page's
    async fn failure_behind(
        &self,
        error: Option<&anyhow::Error>,
        what: &str,
    ) -> Option<anyhow::Error> {
        match error {
            Some(_) if !self.alive().await => Some(Self::failure(what)),
            _ => None,
        }
    }

    /// One request with a deadline, not yet judged. Callers that hold the
    /// browser lock for the request drop it before [`judge`](Self::judge),
    /// which takes the lock again (a writer queued in between would otherwise
    /// wait for the first guard, and the second for the writer).
    async fn attempt<T, E: Into<anyhow::Error>>(
        until: tokio::time::Instant,
        what: &str,
        request: impl std::future::Future<Output = std::result::Result<T, E>>,
    ) -> Result<T> {
        match tokio::time::timeout_at(until, request).await {
            Ok(answer) => answer.map_err(Into::into),
            Err(_) => Err(anyhow::anyhow!("Chrome did not answer in time ({what})")),
        }
    }

    /// After a failed request or navigation: Chrome's failure when it no
    /// longer answers, else `error`, the page's own
    async fn judge(&self, error: anyhow::Error, what: &str) -> anyhow::Error {
        if self.alive().await {
            error
        } else {
            Self::failure(what)
        }
    }

    /// Whether Chrome still has the tab `target`: asked of the browser itself,
    /// within [`ALIVE_LIMIT`]. A crashed tab is still there; a closed one is not.
    /// Like [`alive`](Self::alive), it takes the browser's read lock for the
    /// request alone and judges nothing.
    async fn has_tab(
        &self,
        target: &chromiumoxide::cdp::browser_protocol::target::TargetId,
    ) -> bool {
        let info = chromiumoxide::cdp::browser_protocol::target::GetTargetInfoParams {
            target_id: Some(target.clone()),
        };
        let browser = self.browser.read().await;
        tokio::time::timeout(ALIVE_LIMIT, browser.execute(info))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Open a tab (rules 3, 4 and 6): the one request on the browser itself
    /// whose failure is judged, besides the questions [`alive`](Self::alive) and
    /// [`has_tab`](Self::has_tab). It runs on a task of its own, which holds a read
    /// lock on the browser; the caller waits until `until`, then judges a
    /// failure without holding any lock. Once asked, Chrome makes the tab even
    /// if nobody waits for it any more (the caller ran out of time or was
    /// cancelled), so the task closes such a tab itself, waiting for it at most
    /// [`CLOSE_ACK_LIMIT`] past `until`.
    async fn open_tab(&self, until: tokio::time::Instant) -> Result<Page> {
        let browser = Arc::clone(&self.browser).read_owned().await;
        let (sent, answer) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let opened =
                tokio::time::timeout_at(until + CLOSE_ACK_LIMIT, browser.new_page("about:blank"));
            let opened = opened.await;
            drop(browser);
            if let Err(Ok(Ok(page))) = sent.send(opened) {
                let _ = tokio::time::timeout(CLOSE_ACK_LIMIT, page.close()).await;
            }
        });
        let opened = Self::attempt(until, "opening a tab", answer).await;
        match opened.and_then(|o| Ok(o??)) {
            Ok(page) => Ok(page),
            Err(error) => Err(self.judge(error, "opening a tab").await),
        }
    }

    /// Collect an exited Chrome as soon as its connection is gone, so it
    /// never lingers as a zombie that seems to hold the profile's lock
    async fn collect_exited(&self) {
        self.mark_failed();
        let mut pid = self.pid.lock().await;
        if pid.is_none() {
            return;
        }
        let mut browser = self.browser.write().await;
        let _ = tokio::time::timeout(COLLECT_LIMIT, browser.wait()).await;
        *pid = None;
    }
}

/// Set to `1` or `true` to start Chrome without its sandbox
/// (needed in some containers; ref also falls back automatically).
pub const NO_SANDBOX_ENV: &str = "REF_CHROME_NO_SANDBOX";

/// Browser pool configuration
pub struct BrowserPool {
    chrome: Arc<Chrome>,
    semaphore: Arc<Semaphore>,
    user_agent: String,
    pub(crate) profile: Profile,
    cookies: Vec<CookieParam>,
    cookie_jar: Vec<Cookie>,
    custom_user_agent: bool,
    /// While the pool lives, the final clean-up leaves its fresh profile alone
    _owner: Arc<()>,
}

impl BrowserPool {
    /// Create a new browser pool with concurrency limit
    ///
    /// `profile` is the `--profile` value, or `None` for the default
    /// (see [`profile::choose`]).
    /// A fresh profile is a folder of its own, removed on [`stop`](Self::stop),
    /// so two ref processes never share or lock one profile.
    /// Chrome's sandbox is on unless ref runs as root, [`NO_SANDBOX_ENV`] is set,
    /// or Chrome cannot start with it (then ref retries once without it and says so).
    ///
    /// # Errors
    /// Returns an error if Chrome/Chromium is not found or fails to launch.
    pub async fn new(concurrency: usize, profile: Option<&ProfileChoice>) -> Result<Self> {
        Self::start(concurrency, profile, StartOptions::found()).await
    }

    /// [`new`](Self::new) with the Chrome, sandbox and request budget given
    /// (tests use them to see start-up fail, or what happens when Chrome hangs)
    pub(crate) async fn start(
        concurrency: usize,
        profile: Option<&ProfileChoice>,
        options: StartOptions,
    ) -> Result<Self> {
        let StartOptions {
            chrome: chrome_path,
            sandbox,
            request_timeout,
        } = options;
        let chrome_path = chrome_path.context(
            "Chrome/Chromium not found. Searched paths:\n  \
             Linux: /usr/bin/google-chrome-stable, /usr/bin/google-chrome, /usr/bin/chromium-browser, /usr/bin/chromium\n  \
             macOS: /Applications/Google Chrome.app/...\n  \
             Windows: C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe\n\
             Please install Chrome or Chromium.",
        )?;

        // Reads /proc and removes folders: off the async workers
        let _ = tokio::task::spawn_blocking(sweep_orphans).await;
        let home = profile::home_dir();
        let default_saved = profile::default_saved_dir(home.as_deref());
        let chosen = profile::choose(profile, default_saved.as_deref(), unique_profile_dir);
        // Held by the pool: while it lives, the final clean-up leaves its profile
        let owner = Arc::new(());
        match &chosen {
            Profile::Saved(dir) => {
                prepare_saved_profile(dir, home.as_deref())?;
                eprintln!("Using saved profile {}", dir.display());
            }
            Profile::Fresh(dir) => track_profile(dir, &owner),
        }
        let profile_dir = chosen.path().to_path_buf();

        let launched = match launch(&chrome_path, &profile_dir, sandbox).await {
            Err(e) if sandbox => {
                eprintln!(
                    "Warning: Chrome did not start with its sandbox ({e:#}); retrying without it. \
                     Set {NO_SANDBOX_ENV}=1 to skip this attempt."
                );
                launch(&chrome_path, &profile_dir, false).await
            }
            first => first,
        };
        let (mut browser, handler) = match launched {
            Ok(b) => b,
            Err(e) => {
                start_failed(&chosen).await;
                return Err(e.context(format!(
                    "Chrome did not start (profile {})",
                    profile_dir.display()
                )));
            }
        };
        let pid = browser.get_mut_child().and_then(|c| c.as_mut_inner().id());
        let chrome = Arc::new(Chrome {
            browser: Arc::new(tokio::sync::RwLock::new(browser)),
            pid: tokio::sync::Mutex::new(pid),
            failed: std::sync::atomic::AtomicBool::new(false),
            stopped: std::sync::atomic::AtomicBool::new(false),
            request_timeout,
        });
        listen(handler, Arc::downgrade(&chrome));

        // Rule 6: starting Chrome, up to its first answer
        let version = {
            let browser = chrome.browser.read().await;
            tokio::time::timeout(request_timeout, browser.version()).await
        };
        // Not answering, or gone before answering: no pool
        let Ok(Ok(version)) = version else {
            // Dropping the browser kills this Chrome (chromiumoxide kills on
            // drop); its fresh profile goes with it
            drop(chrome);
            start_failed(&chosen).await;
            anyhow::bail!(
                "Chrome started but did not answer (profile {})",
                profile_dir.display()
            );
        };
        let chrome_ua = version.user_agent;

        // With a saved profile, sites see the same browser you signed in with
        // (sessions and "I am human" passes are often tied to the user agent)
        let user_agent = if chosen.is_saved() {
            chrome_ua.replace("HeadlessChrome", "Chrome")
        } else {
            honest_user_agent(&chrome_ua)
        };

        Ok(Self {
            chrome,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            user_agent,
            profile: chosen,
            cookies: Vec::new(),
            cookie_jar: Vec::new(),
            custom_user_agent: false,
            _owner: owner,
        })
    }

    /// Use this user agent, exactly as given, for every page and every download
    /// from now on. It replaces Chrome's own label with ref's name added.
    ///
    /// # Errors
    /// Returns an error if the text is empty or contains control characters.
    pub fn set_user_agent(&mut self, user_agent: &str) -> Result<()> {
        self.user_agent = validate_user_agent(user_agent)?;
        self.custom_user_agent = true;
        Ok(())
    }

    /// `true` when the user agent was set with [`set_user_agent`](Self::set_user_agent).
    #[must_use]
    pub const fn has_custom_user_agent(&self) -> bool {
        self.custom_user_agent
    }

    /// Send these cookies with every page opened from now on.
    pub fn set_cookies(&mut self, cookies: &[Cookie]) {
        self.cookies = cookies.iter().filter_map(to_cookie_param).collect();
        self.cookie_jar = cookies.to_vec();
    }

    /// Headers for a download outside Chrome that should look like the page
    /// Chrome just loaded: the same user agent and the cookies that apply.
    #[must_use]
    pub fn download_headers(&self, url: &str) -> DownloadHeaders {
        self.download_headers_as(url, None)
    }

    /// [`download_headers`](Self::download_headers) for a page opened with
    /// [`new_page_as`](Self::new_page_as): that tab's user agent
    #[must_use]
    pub fn download_headers_as(&self, url: &str, user_agent: Option<&str>) -> DownloadHeaders {
        DownloadHeaders {
            user_agent: user_agent.unwrap_or(&self.user_agent).to_string(),
            cookie: url::Url::parse(url)
                .ok()
                .and_then(|u| crate::cookies::header_for(&self.cookie_jar, &u)),
        }
    }

    /// True while Chrome answers within [`ALIVE_LIMIT`] (ADR-005 rule 2). A
    /// crashed or killed Chrome fails at once, a frozen one after the limit;
    /// either way the pool is then failed for good.
    pub async fn is_alive(&self) -> bool {
        self.chrome.alive().await
    }

    /// Get a new page with resource blocking and Chrome's cache turned off, so
    /// every navigation asks the site again (a long-lived browser would
    /// otherwise answer from its cache).
    ///
    /// # Errors
    /// Returns [`ChromeFailed`] when Chrome failed, or an error when the tab
    /// could not be set up (it is closed then).
    pub async fn new_page(&self) -> Result<BrowserPage> {
        self.new_page_as(None).await
    }

    /// [`new_page`](Self::new_page) with `user_agent` for this tab alone, exactly
    /// as given (already checked by [`validate_user_agent`]), in place of the
    /// pool's: a call on a shared browser (the MCP server) sets its own without
    /// changing what other calls send.
    ///
    /// # Errors
    /// The same as [`new_page`](Self::new_page).
    pub async fn new_page_as(&self, user_agent: Option<&str>) -> Result<BrowserPage> {
        // A failed pool fails at once, also when every tab slot is taken
        if self.chrome.has_failed() {
            return Err(Chrome::failure("opening a tab"));
        }
        let permit = self.semaphore.clone().acquire_owned().await?;
        // The pool may have failed while this call waited for a slot
        if self.chrome.has_failed() {
            return Err(Chrome::failure("opening a tab"));
        }
        // One budget for opening the tab and setting it up (rule 6)
        let until = tokio::time::Instant::now() + self.chrome.request_timeout;
        let page = self.chrome.open_tab(until).await?;
        // A tab closes when dropped, so one whose set-up fails, runs out of
        // time or is cancelled is closed too: a long-lived browser does not
        // collect half-made tabs (rule 6)
        let tab = BrowserPage {
            page,
            permit: Some(permit),
            chrome: Arc::clone(&self.chrome),
            reads_until: std::sync::Mutex::new(None),
        };
        let agent = user_agent.unwrap_or(&self.user_agent);
        self.set_up(&tab.page, agent, until).await?;
        Ok(tab)
    }

    async fn set_up(&self, page: &Page, agent: &str, until: tokio::time::Instant) -> Result<()> {
        // Each step on one line: a `?` alone on its line counts as a line
        // missed whenever that step happens not to fail
        let chrome = &self.chrome;
        let agent = page.execute(SetUserAgentOverrideParams::new(agent));
        chrome.ask(until, "setting the user agent", agent).await?;

        if !self.cookies.is_empty() {
            // Raw CDP call: chromiumoxide's helper refuses to run on about:blank
            let cookies = page.execute(SetCookiesParams::new(self.cookies.clone()));
            let set = chrome.ask(until, "setting cookies", cookies).await;
            set.context("Failed to set cookies")?;
        }

        let no_cache = page.execute(SetCacheDisabledParams::new(true));
        chrome.ask(until, "turning off the cache", no_cache).await?;

        // Enable lifecycle events for networkIdle detection
        let lifecycle = page.execute(SetLifecycleEventsEnabledParams::new(true));
        chrome.ask(until, "enabling page events", lifecycle).await?;
        Ok(())
    }

    /// Stop Chrome (ADR-005 rule 4). A Chrome that answers is asked to close
    /// and given [`EXIT_LIMIT`] to exit, since it may be saving a saved profile;
    /// a failed one, or one that does not close, is killed. Then the process is
    /// collected and a fresh profile folder removed. Works while others still
    /// hold the pool; stopping again does nothing.
    /// It cannot fail, so a run's own error is never replaced by one from here.
    pub async fn stop(&self) {
        let mut pid = self.chrome.pid.lock().await;
        if self
            .chrome
            .stopped
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let failed = self.chrome.has_failed();
        if failed {
            // Ends requests in flight at once, so the browser is free sooner
            if let Some(pid) = *pid {
                kill_process(pid);
            }
        }
        let mut browser = self.chrome.browser.write().await;
        let closed = !failed && close_gracefully(&mut browser).await;
        if !closed {
            let _ = tokio::time::timeout(COLLECT_LIMIT, browser.kill()).await;
        }
        if !closed && !failed {
            eprintln!("Warning: Chrome did not close in time, so it was stopped.");
            if self.profile.is_saved() {
                eprintln!("Sign-ins from this run may not have been saved.");
            }
        }
        *pid = None;
        self.chrome
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        drop(browser);
        if let Profile::Fresh(dir) = &self.profile {
            remove_profile(dir).await;
        }
    }

    /// [`stop`](Self::stop), taking the pool
    pub async fn close(self) {
        self.stop().await;
    }
}

/// Ask Chrome to close and wait for it to exit: true when it did both in time
async fn close_gracefully(browser: &mut Browser) -> bool {
    let acked = tokio::time::timeout(CLOSE_ACK_LIMIT, browser.close())
        .await
        .is_ok_and(|r| r.is_ok());
    acked
        && tokio::time::timeout(EXIT_LIMIT, browser.wait())
            .await
            .is_ok()
}

/// How a pool starts Chrome
pub(crate) struct StartOptions {
    /// Chrome's program, `None` when none was found
    pub(crate) chrome: Option<PathBuf>,
    /// Try Chrome's sandbox first (then once without it, if Chrome fails)
    pub(crate) sandbox: bool,
    /// The request budget of rule 6
    pub(crate) request_timeout: Duration,
}

impl StartOptions {
    /// The Chrome found on this system; the sandbox unless ref runs as root
    /// or [`NO_SANDBOX_ENV`] is set
    pub(crate) fn found() -> Self {
        Self {
            chrome: detect_chrome_path(),
            sandbox: want_sandbox(
                std::env::var(NO_SANDBOX_ENV).ok().as_deref(),
                running_as_root(),
            ),
            request_timeout: CHROME_REQUEST_TIMEOUT,
        }
    }
}

/// Clean up after a Chrome that did not start: a fresh profile is removed,
/// a saved one is the user's and stays
async fn start_failed(chosen: &Profile) {
    if let Profile::Fresh(dir) = chosen {
        remove_profile(dir).await;
    }
}

/// Remove a fresh profile folder, waiting and retrying after a kill (see
/// [`remove_dir_retrying`]); it stays on the list for the final clean-up until
/// it is gone.
async fn remove_profile(dir: &Path) {
    let owned = dir.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || remove_dir_retrying(&owned)).await;
    untrack_profile(dir);
}

/// Read Chrome's messages until the connection fails (messages it cannot
/// parse are skipped, not errors); then the pool is failed and the exited
/// Chrome collected, so every request fails at once instead of waiting.
fn listen(mut handler: chromiumoxide::Handler, chrome: std::sync::Weak<Chrome>) {
    tokio::spawn(async move {
        while let Some(Ok(())) = handler.next().await {}
        // Dropping the handler fails every request still waiting for an
        // answer at once; collecting may have to wait for them
        drop(handler);
        if let Some(chrome) = chrome.upgrade() {
            chrome.collect_exited().await;
        }
    });
}

impl Drop for BrowserPool {
    fn drop(&mut self) {
        // A pool that was never stopped: Chrome is killed when the last tab
        // drops it (chromiumoxide kills on drop), and may still write to the
        // profile while it exits; the folder stays tracked so the final
        // `remove_open_profiles` call in main removes it after that.
        if let Profile::Fresh(dir) = &self.profile {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Request headers for a download made outside Chrome
#[derive(Debug, Clone, Default)]
pub struct DownloadHeaders {
    pub user_agent: String,
    pub cookie: Option<String>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::process::process_alive;
    use super::*;

    /// Chrome as found, with request budgets of `budget`
    fn with_budget(budget: Duration) -> StartOptions {
        StartOptions {
            request_timeout: budget,
            ..StartOptions::found()
        }
    }
    use std::io::{BufRead, BufReader, Write};

    pub(crate) const FRESH: Option<&ProfileChoice> = Some(&ProfileChoice::Fresh);

    impl BrowserPool {
        /// The process id of this pool's Chrome, until it has been collected
        pub(crate) async fn chrome_pid(&self) -> Option<u32> {
            *self.chrome.pid.lock().await
        }

        /// Kill this pool's Chrome outright, as a crash would, and wait until
        /// ref has noticed and collected it
        pub(crate) async fn kill_chrome(&self) {
            signal_chrome(self, "-KILL").await;
            wait_for(|| async { self.chrome_pid().await.is_none() }).await;
        }

        /// Freeze this pool's Chrome (SIGSTOP), as a hang would: it keeps
        /// running but answers nothing
        pub(crate) async fn freeze_chrome(&self) {
            signal_chrome(self, "-STOP").await;
        }
    }

    impl BrowserPage {
        /// Close this tab from Chrome's side, as a tab crash would end it,
        /// while Chrome itself stays fine
        pub(crate) async fn end_tab_in_chrome(&self) {
            let target = self.page.target_id().clone();
            let close =
                chromiumoxide::cdp::browser_protocol::target::CloseTargetParams::new(target);
            let browser = self.chrome.browser.read().await;
            browser.execute(close).await.unwrap();
        }
    }

    async fn signal_chrome(pool: &BrowserPool, signal: &str) {
        let pid = pool.chrome_pid().await.expect("Chrome runs");
        let sent = std::process::Command::new("kill")
            .args([signal, &pid.to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
    }

    /// Wait (checking every 50 ms, for at most 10 s) until `done` says so
    pub(crate) async fn wait_for<F: std::future::Future<Output = bool>>(done: impl Fn() -> F) {
        let checks = futures::stream::repeat(()).then(|()| async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            done().await
        });
        let waited = tokio::time::timeout(Duration::from_secs(10), checks.any(std::future::ready));
        assert!(waited.await.unwrap_or(false), "gave up waiting");
    }

    /// Serve a page that sends `signal` to `pool`'s Chrome when Chrome asks
    /// for it, then answers nothing: Chrome fails in the middle of navigation
    pub(crate) async fn serve_trap(pool: &BrowserPool, signal: &'static str) -> String {
        let pid = pool.chrome_pid().await.expect("Chrome runs");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = std::process::Command::new("kill")
                    .args([signal, &pid.to_string()])
                    .status();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(30));
                    drop(stream);
                });
            }
        });
        format!("http://{addr}/trap")
    }

    /// Serve one page that tells the browser to cache it for an hour, with a
    /// number that goes up on every request
    fn serve_cacheable_counter() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut visits = 0;
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut first, mut line) = (String::new(), String::new());
                let _ = reader.read_line(&mut first);
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                // Only the page counts: Chrome may also ask for /favicon.ico
                let n = visits;
                if first.contains("/page") {
                    visits += 1;
                }
                let body = format!("<html><body><p>visit {n}</p></body></html>");
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: max-age=3600\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let mut stream = stream;
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        format!("http://{addr}/page")
    }

    #[tokio::test]
    async fn pages_ask_the_site_again_instead_of_the_cache() {
        // Needs Chrome (ADR-004)
        let url = serve_cacheable_counter();
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        let mut seen = Vec::new();
        for _ in 0..2 {
            let page = pool.new_page().await.unwrap();
            page.goto(&url, 10_000).await.unwrap();
            seen.push(page.content().await.unwrap());
        }
        pool.close().await;
        let (first, second) = (&seen[0], &seen[1]);
        assert!(first.contains("visit 0"), "{first}");
        assert!(
            !second.contains("visit 0"),
            "served from the cache: {second}"
        );
    }

    #[tokio::test]
    async fn rule_1_stopping_a_crashed_pool_leaves_a_later_chrome_alone() {
        // Needs Chrome (ADR-004). Before, ref killed whatever the lock named.
        let dir = tempfile::tempdir().unwrap();
        let saved = ProfileChoice::Folder(dir.path().join("saved"));
        let crashed = BrowserPool::new(1, Some(&saved)).await.unwrap();
        crashed.kill_chrome().await;

        // Collected at once, so the profile is free for the next Chrome
        let later = BrowserPool::new(1, Some(&saved)).await.unwrap();
        crashed.stop().await;
        assert!(
            later.is_alive().await,
            "stopping the crashed pool hit the later Chrome"
        );
        later.stop().await;
    }

    #[tokio::test]
    async fn rule_2_dead_is_told_from_alive_at_once_and_frozen_within_the_limit() {
        // Needs Chrome (ADR-004)
        let killed = BrowserPool::new(1, FRESH).await.unwrap();
        assert!(killed.is_alive().await);
        killed.kill_chrome().await;
        let started = std::time::Instant::now();
        assert!(!killed.is_alive().await);
        let took = started.elapsed();
        assert!(took < Duration::from_secs(1), "{took:?}");

        let frozen = BrowserPool::new(1, FRESH).await.unwrap();
        frozen.freeze_chrome().await;
        let started = std::time::Instant::now();
        assert!(!frozen.is_alive().await);
        let took = started.elapsed();
        assert!(took >= ALIVE_LIMIT && took < ALIVE_LIMIT * 2, "{took:?}");
        frozen.stop().await;
    }

    #[tokio::test]
    async fn rule_3_chrome_killed_during_navigation_fails_chrome_not_the_url() {
        // Needs Chrome (ADR-004)
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        let trap = serve_trap(&pool, "-KILL").await;
        let page = pool.new_page().await.unwrap();
        let err = page.goto(&trap, 10_000).await.unwrap_err();
        assert!(err.is::<ChromeFailed>(), "{err:#}");

        // A failed pool fails its next request at once
        let started = std::time::Instant::now();
        let next = pool.new_page().await.err().expect("refused");
        assert!(next.is::<ChromeFailed>(), "{next:#}");
        assert!(started.elapsed() < Duration::from_millis(500));
        pool.stop().await;
    }

    #[tokio::test]
    async fn rule_3_requests_waiting_on_chrome_fail_at_once_when_it_dies() {
        // Needs Chrome (ADR-004). Before, they waited for their 30 s budget.
        let pool = Arc::new(BrowserPool::new(1, FRESH).await.unwrap());
        pool.freeze_chrome().await;
        let waiting = Arc::clone(&pool);
        let opening = tokio::spawn(async move { waiting.new_page().await.err() });
        tokio::time::sleep(Duration::from_millis(500)).await;
        let started = std::time::Instant::now();
        signal_chrome(&pool, "-KILL").await;
        let err = opening.await.unwrap().expect("opening a tab failed");
        assert!(err.is::<ChromeFailed>(), "{err:#}");
        let took = started.elapsed();
        assert!(took < Duration::from_secs(10), "{took:?}");
        pool.stop().await;
    }

    #[tokio::test]
    async fn rule_4_stopping_while_a_tab_opens_on_a_frozen_chrome_ends() {
        // Needs Chrome (ADR-004). Before, the tab opening and the stop could
        // wait for each other forever.
        let budget = Duration::from_secs(2);
        let pool = Arc::new(
            BrowserPool::start(1, FRESH, with_budget(budget))
                .await
                .unwrap(),
        );
        pool.freeze_chrome().await;
        let waiting = Arc::clone(&pool);
        let opening = tokio::spawn(async move { waiting.new_page().await.err() });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let both = async {
            pool.stop().await;
            opening.await.unwrap()
        };
        let err = tokio::time::timeout(Duration::from_secs(20), both).await;
        let err = err.expect("stop and the opening tab waited for each other");
        assert!(err.expect("opening a tab failed").is::<ChromeFailed>());
    }

    #[tokio::test]
    async fn rule_6_a_chrome_that_does_not_answer_at_start_leaves_no_profile() {
        // Needs Chrome (ADR-004). A zero budget: the first answer cannot come in time.
        let err = BrowserPool::start(1, FRESH, with_budget(Duration::ZERO)).await;
        let err = err.err().expect("start failed").to_string();
        assert!(
            err.starts_with("Chrome started but did not answer"),
            "{err}"
        );
        let dir = err.split("(profile ").nth(1).unwrap().trim_end_matches(')');
        assert!(!Path::new(dir).exists(), "{dir} was left behind");

        // A saved profile is the user's: it stays
        let home = tempfile::tempdir().unwrap();
        let saved = ProfileChoice::Folder(home.path().join("saved"));
        let err = BrowserPool::start(1, Some(&saved), with_budget(Duration::ZERO)).await;
        assert!(err.is_err());
        assert!(
            home.path().join("saved").is_dir(),
            "the saved profile was removed"
        );
    }

    #[tokio::test]
    async fn without_chrome_no_pool_starts() {
        let none = StartOptions {
            chrome: None,
            ..StartOptions::found()
        };
        let err = BrowserPool::start(1, FRESH, none).await.err().unwrap();
        assert!(
            err.to_string().starts_with("Chrome/Chromium not found"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_chrome_that_does_not_start_is_tried_without_its_sandbox_and_leaves_no_profile() {
        // `false` stands in for a Chrome that exits at once
        let failing = |sandbox| StartOptions {
            chrome: Some(PathBuf::from("/usr/bin/false")),
            sandbox,
            request_timeout: CHROME_REQUEST_TIMEOUT,
        };
        for sandbox in [true, false] {
            let err = BrowserPool::start(1, FRESH, failing(sandbox)).await;
            let err = err.err().expect("a pool started");
            let message = err.to_string();
            assert!(
                message.starts_with("Chrome did not start (profile "),
                "{message}"
            );
            assert!(
                format!("{err:#}").contains("Failed to launch Chrome"),
                "{err:#}"
            );
            let dir = message
                .split("(profile ")
                .nth(1)
                .unwrap()
                .trim_end_matches(')');
            assert!(!Path::new(dir).exists(), "{dir} was left behind");
            assert!(
                !process::is_tracked(Path::new(dir)),
                "{dir} is still tracked"
            );
        }

        // A saved profile is the user's: it stays
        let home = tempfile::tempdir().unwrap();
        let saved = ProfileChoice::Folder(home.path().join("saved"));
        let err = BrowserPool::start(1, Some(&saved), failing(false)).await;
        assert!(err.is_err());
        assert!(
            home.path().join("saved").is_dir(),
            "the saved profile was removed"
        );
    }

    #[tokio::test]
    async fn rule_6_a_failed_navigation_also_starts_a_read_budget() {
        // Needs Chrome (ADR-004)
        let budget = Duration::from_secs(2);
        let pool = BrowserPool::start(1, FRESH, with_budget(budget))
            .await
            .unwrap();
        let page = pool.new_page().await.unwrap();
        page.goto("data:text/html,<p>here</p>", 10_000)
            .await
            .unwrap();
        tokio::time::sleep(budget + Duration::from_millis(500)).await;
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let failed = page
            .goto(&format!("http://127.0.0.1:{port}/"), 10_000)
            .await
            .unwrap();
        assert!(failed.error.is_some());
        // The first navigation's budget is long spent: reading still works
        assert!(page.current_url().await.unwrap().is_some());
        drop(page);
        pool.stop().await;
    }

    #[tokio::test]
    async fn rule_6_every_navigation_has_its_own_read_budget() {
        // Needs Chrome (ADR-004). Before, a second navigation on the same tab
        // read against the first one's budget, long spent.
        let budget = Duration::from_secs(2);
        let pool = BrowserPool::start(1, FRESH, with_budget(budget))
            .await
            .unwrap();
        let page = pool.new_page().await.unwrap();
        let url = "data:text/html,<title>Twice</title><p>here</p>";
        page.goto(url, 10_000).await.unwrap();
        tokio::time::sleep(budget + Duration::from_millis(500)).await;
        let again = page.goto(url, 10_000).await.unwrap();
        assert_eq!(again.title.as_deref(), Some("Twice"));
        assert!(page.current_url().await.unwrap().is_some());
        drop(page);
        pool.stop().await;
    }

    #[tokio::test]
    async fn rule_3_a_call_waiting_for_a_slot_learns_the_pool_failed() {
        // Needs Chrome (ADR-004). Before, it asked the dead Chrome for the full budget.
        let pool = Arc::new(BrowserPool::new(1, FRESH).await.unwrap());
        let holder = pool.new_page().await.unwrap();
        let waiting = Arc::clone(&pool);
        let queued = tokio::spawn(async move { waiting.new_page().await.err() });
        tokio::time::sleep(Duration::from_millis(300)).await;
        pool.kill_chrome().await;
        let started = std::time::Instant::now();
        drop(holder);
        let err = queued.await.unwrap().expect("the queued call failed");
        assert!(err.is::<ChromeFailed>(), "{err:#}");
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "{took:?}");
        pool.stop().await;
    }

    #[tokio::test]
    async fn rule_3_chrome_frozen_during_navigation_fails_chrome_not_the_url() {
        // Needs Chrome (ADR-004)
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        let trap = serve_trap(&pool, "-STOP").await;
        let page = pool.new_page().await.unwrap();
        let err = page.goto(&trap, 2_000).await.unwrap_err();
        assert!(err.is::<ChromeFailed>(), "{err:#}");
        drop(page);
        pool.stop().await;
    }

    #[tokio::test]
    async fn rule_3_a_refused_connection_is_still_the_pages_failure() {
        // Needs Chrome (ADR-004)
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        let page = pool.new_page().await.unwrap();
        // A port that was just free, so nothing listens there
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let nav = page
            .goto(&format!("http://127.0.0.1:{port}/"), 10_000)
            .await
            .unwrap();
        let error = nav.error.unwrap_or_default();
        assert!(error.starts_with("CONNECTION_REFUSED"), "{error}");
        assert!(pool.is_alive().await);
        drop(page);
        pool.stop().await;
    }

    /// Serve `/set`, which sets a cookie for an hour, and `/show`, which shows
    /// the cookies it was sent
    pub(crate) fn serve_cookie_jar() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut first, mut cookie, mut line) =
                    (String::new(), String::new(), String::new());
                let _ = reader.read_line(&mut first);
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("cookie:") {
                        cookie = value.trim().to_string();
                    }
                    line.clear();
                }
                let set = if first.contains("/set") {
                    "Set-Cookie: kept=yes; Max-Age=3600; Path=/\r\n"
                } else {
                    ""
                };
                let body = format!("<html><body><p>cookies: {cookie}</p></body></html>");
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{set}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let mut stream = stream;
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn rule_4_a_graceful_stop_keeps_the_sign_ins_of_a_saved_profile() {
        // Needs Chrome (ADR-004). Before, a Chrome slower than 5 s to exit was killed.
        let site = serve_cookie_jar();
        let dir = tempfile::tempdir().unwrap();
        let saved = ProfileChoice::Folder(dir.path().join("saved"));

        let first = BrowserPool::new(1, Some(&saved)).await.unwrap();
        let page = first.new_page().await.unwrap();
        page.goto(&format!("{site}/set"), 10_000).await.unwrap();
        drop(page);
        first.stop().await;

        let second = BrowserPool::new(1, Some(&saved)).await.unwrap();
        let page = second.new_page().await.unwrap();
        page.goto(&format!("{site}/show"), 10_000).await.unwrap();
        let html = page.content().await.unwrap();
        assert!(html.contains("kept=yes"), "the sign-in was lost: {html}");
        drop(page);
        second.stop().await;
    }

    #[tokio::test]
    async fn rule_4_a_frozen_chrome_is_killed_and_its_profile_removed() {
        // Needs Chrome (ADR-004)
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        let (pid, dir) = (
            pool.chrome_pid().await.unwrap(),
            pool.profile.path().to_path_buf(),
        );
        pool.freeze_chrome().await;
        let started = std::time::Instant::now();
        pool.stop().await;
        let took = started.elapsed();
        assert!(
            took < CLOSE_ACK_LIMIT + COLLECT_LIMIT + Duration::from_secs(3),
            "{took:?}"
        );
        assert!(!process_alive(pid), "the frozen Chrome still runs");
        assert!(!dir.exists(), "the fresh profile was not removed");
        assert_eq!(pool.chrome_pid().await, None);
    }

    #[tokio::test]
    async fn rule_4_a_frozen_chrome_on_a_saved_profile_is_killed_with_a_warning() {
        // Needs Chrome (ADR-004). The warning goes to stderr; this checks the kill.
        let dir = tempfile::tempdir().unwrap();
        let saved = ProfileChoice::Folder(dir.path().join("saved"));
        let pool = BrowserPool::new(1, Some(&saved)).await.unwrap();
        let pid = pool.chrome_pid().await.unwrap();
        pool.freeze_chrome().await;
        pool.stop().await;
        assert!(!process_alive(pid), "the frozen Chrome still runs");
        assert!(dir.path().join("saved").is_dir(), "a saved profile is kept");
    }

    #[tokio::test]
    async fn rule_4_a_pool_dropped_without_stopping_still_ends_its_chrome() {
        // Needs Chrome (ADR-004): the backstop, not the normal path
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        let pid = pool.chrome_pid().await.unwrap();
        drop(pool);
        wait_for(|| async { !process_alive(pid) }).await;
    }

    #[tokio::test]
    async fn rule_4_stopping_works_while_shared_and_twice() {
        // Needs Chrome (ADR-004)
        let pool = Arc::new(BrowserPool::new(1, FRESH).await.unwrap());
        let other = Arc::clone(&pool);
        pool.stop().await;
        assert!(
            !other.is_alive().await,
            "still running while another holder has it"
        );
        other.stop().await;
        let refused = other
            .new_page()
            .await
            .err()
            .expect("a stopped pool opens no tab");
        assert!(refused.is::<ChromeFailed>(), "{refused:#}");
    }

    #[tokio::test]
    async fn rule_6_reads_of_a_hung_page_share_one_budget() {
        // Needs Chrome (ADR-004). Before, each read had a limit of its own.
        let budget = Duration::from_secs(2);
        let pool = BrowserPool::start(1, FRESH, with_budget(budget))
            .await
            .unwrap();
        let page = pool.new_page().await.unwrap();
        let nav = page
            .goto("data:text/html,<p>here</p>", 10_000)
            .await
            .unwrap();
        assert!(nav.error.is_none(), "{nav:?}");
        pool.freeze_chrome().await;
        let started = std::time::Instant::now();
        let err = page.content().await.unwrap_err();
        assert!(err.is::<ChromeFailed>(), "{err:#}");
        // One budget (2 s) and one check that Chrome is alive (5 s), not one per read
        let took = started.elapsed();
        assert!(
            took < budget + ALIVE_LIMIT + Duration::from_secs(2),
            "{took:?}"
        );
        drop(page);
        pool.stop().await;
    }

    /// A pool whose tabs Chrome refuses to set up (a cookie with ';' in its
    /// name), while Chrome itself is fine: the page's failure, not Chrome's
    pub(crate) async fn pool_that_cannot_set_up_tabs() -> Arc<BrowserPool> {
        let mut pool = BrowserPool::new(1, FRESH).await.unwrap();
        pool.set_cookies(&[Cookie {
            domain: "127.0.0.1".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            expires: None,
            name: "bad;name".into(),
            value: "v".into(),
        }]);
        Arc::new(pool)
    }

    /// The tabs open in Chrome, as Chrome itself counts them
    async fn open_tabs(pool: &BrowserPool) -> usize {
        let targets = chromiumoxide::cdp::browser_protocol::target::GetTargetsParams::default();
        let browser = pool.chrome.browser.read().await;
        let answer = browser.execute(targets).await.unwrap();
        answer
            .result
            .target_infos
            .iter()
            .filter(|t| t.r#type == "page")
            .count()
    }

    #[tokio::test]
    async fn rule_6_a_tab_that_fails_to_set_up_is_closed() {
        // Needs Chrome (ADR-004). Chrome refuses a cookie with a ';' in its name.
        let mut pool = BrowserPool::new(1, FRESH).await.unwrap();
        let tabs_before = open_tabs(&pool).await;
        pool.set_cookies(&[Cookie {
            domain: "example.com".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            expires: None,
            name: "bad;name".into(),
            value: "v".into(),
        }]);
        let err = pool.new_page().await.err().expect("set-up failed");
        assert!(err.to_string().contains("cookies"), "{err:#}");
        assert!(!err.is::<ChromeFailed>(), "Chrome is fine: {err:#}");
        // Chrome acknowledges the close before its list of tabs catches up
        wait_for(|| async { open_tabs(&pool).await == tabs_before }).await;
        pool.stop().await;
    }

    #[tokio::test]
    async fn rule_6_a_tab_chrome_makes_after_its_deadline_is_closed() {
        // Needs Chrome (ADR-004). A deadline already past: the caller gives
        // up at once, while Chrome, which answers, makes the tab anyway.
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        let tabs_before = open_tabs(&pool).await;
        let err = pool.chrome.open_tab(tokio::time::Instant::now()).await;
        let err = err.expect_err("out of time");
        assert!(err.to_string().contains("in time"), "{err:#}");
        assert!(!err.is::<ChromeFailed>(), "Chrome is fine: {err:#}");
        // Made after the caller gave up, then closed
        tokio::time::sleep(Duration::from_millis(500)).await;
        wait_for(|| async { open_tabs(&pool).await == tabs_before }).await;
        pool.stop().await;
    }

    #[tokio::test]
    async fn rule_6_a_tab_whose_caller_is_cancelled_is_closed() {
        // Needs Chrome (ADR-004). The call is dropped at points spread over
        // opening the tab and setting it up; no tab is left either way.
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        let tabs_before = open_tabs(&pool).await;
        for ms in [0, 2, 5, 10, 20, 40] {
            let cut = Duration::from_millis(ms);
            let _ = tokio::time::timeout(cut, pool.new_page()).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        wait_for(|| async { open_tabs(&pool).await == tabs_before }).await;
        // The slots were all freed: a tab still opens
        drop(pool.new_page().await.unwrap());
        pool.stop().await;
    }

    #[tokio::test]
    async fn a_downloads_user_agent_is_the_tabs_own_when_it_has_one() {
        // Needs Chrome (ADR-004)
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        let url = "https://example.com/report.pdf";
        let own = pool.download_headers(url).user_agent;
        assert!(own.contains("mollendorff-ref/"), "{own}");
        assert_eq!(pool.download_headers_as(url, None).user_agent, own);
        let declared = pool.download_headers_as(url, Some("Jane Doe jane@example.com"));
        assert_eq!(declared.user_agent, "Jane Doe jane@example.com");
        pool.stop().await;
    }

    #[test]
    fn page_failures_are_split_from_chrome_failures() {
        let ok: Result<u8> = Ok(1);
        assert_eq!(page_failure(ok).unwrap(), Ok(1));
        let page: Result<u8> = Err(anyhow::anyhow!("site said no"));
        assert_eq!(page_failure(page).unwrap(), Err("site said no".to_string()));
        let chrome: Result<u8> = Err(anyhow::Error::new(ChromeFailed("gone".into())));
        let err = page_failure(chrome).unwrap_err();
        assert_eq!(err.to_string(), "Chrome failed: gone");
    }

    #[tokio::test]
    async fn profile_value_chooses_the_chrome_profile() {
        // Needs Chrome, like every browser test (ADR-004)
        let dir = tempfile::tempdir().unwrap();
        let saved = dir.path().join("saved");

        let pool = BrowserPool::new(1, Some(&ProfileChoice::Folder(saved.clone())))
            .await
            .unwrap();
        assert_eq!(pool.profile, Profile::Saved(saved.clone()));
        pool.close().await;
        assert!(saved.is_dir(), "a saved profile is kept after close");

        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        assert!(!pool.profile.is_saved(), "{:?}", pool.profile);
        let temp = pool.profile.path().to_path_buf();
        assert_ne!(temp, saved);
        pool.close().await;
        assert!(!temp.exists(), "a fresh profile is removed on close");
    }
}
