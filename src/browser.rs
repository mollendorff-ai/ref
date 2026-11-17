//! Headless Chrome browser management via chromiumoxide
//!
//! v1.2.0: Added `networkIdle` wait for SPA support (ADR-002)

use crate::cookies::Cookie;
use crate::profile::{self, Profile, ProfileChoice};
use anyhow::{Context, Result};
use chromiumoxide::browser::BrowserConfigBuilder;
use chromiumoxide::cdp::browser_protocol::network::{
    CookieParam, EventResponseReceived, ResourceType, SetCacheDisabledParams, SetCookiesParams,
    TimeSinceEpoch,
};
use chromiumoxide::cdp::browser_protocol::page::{
    EventLifecycleEvent, SetLifecycleEventsEnabledParams,
};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::{FutureExt, StreamExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Default wait time for network idle (ms) - how long to wait for `networkIdle` event
const NETWORK_IDLE_TIMEOUT_MS: u64 = 10000;

/// Auto-detect Chrome/Chromium executable path based on OS
pub(crate) fn detect_chrome_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let candidates = [
        "/usr/bin/google-chrome-stable",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium-browser",
        "/usr/bin/chromium",
        "/snap/bin/chromium",
        "/usr/local/bin/chrome",
        "/usr/local/bin/chromium",
    ];

    #[cfg(target_os = "macos")]
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    ];

    #[cfg(target_os = "windows")]
    let candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        &format!(
            r"{}\Google\Chrome\Application\chrome.exe",
            std::env::var("LOCALAPPDATA").unwrap_or_default()
        ),
    ];

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let candidates: [&str; 0] = [];

    for path in candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Profile folders of pools that are still open, so an interrupted run can
/// remove them (see [`remove_open_profiles`]).
static OPEN_PROFILES: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn track_profile(dir: &Path, open: bool) {
    if let Ok(mut dirs) = OPEN_PROFILES.lock() {
        dirs.retain(|d| d != dir);
        if open {
            dirs.push(dir.to_path_buf());
        }
    }
}

/// Remove the temporary Chrome profiles of pools that were not closed.
/// Called when ref is interrupted (Ctrl-C or SIGTERM).
pub fn remove_open_profiles() {
    if let Ok(mut dirs) = OPEN_PROFILES.lock() {
        if dirs.is_empty() {
            return;
        }
        // A killed Chrome can still write to its profile for a moment
        // while it exits, so remove, wait, and remove again.
        for _ in 0..3 {
            for dir in dirs.iter() {
                let _ = std::fs::remove_dir_all(dir);
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        dirs.retain(|d| d.exists());
    }
}

/// How long ref waits for Chrome to answer one request, the one limit on every
/// request (see [`within`]). A Chrome that crashed fails at once; this limit is
/// for one that hangs.
const CHROME_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Every request to Chrome goes through here. chromiumoxide has a request
/// timeout too, but it does not fire while Chrome is frozen (seen with
/// SIGSTOP), so without this a hung Chrome would hold the call forever.
async fn within<T, E: Into<anyhow::Error>>(
    limit: Duration,
    what: &str,
    request: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    match tokio::time::timeout(limit, request).await {
        Ok(answer) => answer.map_err(Into::into),
        Err(_) => Err(anyhow::anyhow!(
            "Chrome did not answer within {} s ({what})",
            limit.as_secs()
        )),
    }
}

/// How long [`BrowserPool::close`] waits for Chrome to exit before killing it
const CHROME_EXIT_WAIT: Duration = Duration::from_secs(5);

/// Set to `1` or `true` to start Chrome without its sandbox
/// (needed in some containers; ref also falls back automatically).
pub const NO_SANDBOX_ENV: &str = "REF_CHROME_NO_SANDBOX";

/// Browser pool configuration
pub struct BrowserPool {
    browser: Browser,
    semaphore: Arc<Semaphore>,
    user_agent: String,
    pub(crate) profile: Profile,
    /// Limit on each request to Chrome (see [`within`])
    request_timeout: Duration,
    cookies: Vec<CookieParam>,
    cookie_jar: Vec<Cookie>,
    custom_user_agent: bool,
}

impl BrowserPool {
    /// Create a new browser pool with concurrency limit
    ///
    /// `profile` is the `--profile` value, or `None` for the default
    /// (see [`profile::choose`]).
    /// A fresh profile is a folder of its own, removed on [`close`](Self::close),
    /// so two ref processes never share or lock one profile.
    /// Chrome's sandbox is on unless ref runs as root, [`NO_SANDBOX_ENV`] is set,
    /// or Chrome cannot start with it (then ref retries once without it and says so).
    ///
    /// # Errors
    /// Returns an error if Chrome/Chromium is not found or fails to launch.
    pub async fn new(concurrency: usize, profile: Option<&ProfileChoice>) -> Result<Self> {
        Self::with_request_timeout(concurrency, profile, CHROME_REQUEST_TIMEOUT).await
    }

    /// [`new`](Self::new) with another limit on each request to Chrome (tests
    /// use a short one to see what happens when Chrome hangs)
    pub(crate) async fn with_request_timeout(
        concurrency: usize,
        profile: Option<&ProfileChoice>,
        request_timeout: Duration,
    ) -> Result<Self> {
        let chrome_path = detect_chrome_path().ok_or_else(|| {
            anyhow::anyhow!(
                "Chrome/Chromium not found. Searched paths:\n  \
                 Linux: /usr/bin/google-chrome-stable, /usr/bin/google-chrome, /usr/bin/chromium-browser, /usr/bin/chromium\n  \
                 macOS: /Applications/Google Chrome.app/...\n  \
                 Windows: C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe\n\
                 Please install Chrome or Chromium."
            )
        })?;

        sweep_orphans();
        let home = profile::home_dir();
        let default_saved = profile::default_saved_dir(home.as_deref());
        let chosen = profile::choose(profile, default_saved.as_deref(), unique_profile_dir);
        match &chosen {
            Profile::Saved(dir) => {
                prepare_saved_profile(dir, home.as_deref())?;
                eprintln!("Using saved profile {}", dir.display());
            }
            Profile::Fresh(dir) => track_profile(dir, true),
        }
        let profile_dir = chosen.path().to_path_buf();
        let sandbox = want_sandbox(
            std::env::var(NO_SANDBOX_ENV).ok().as_deref(),
            running_as_root(),
        );

        let browser = match launch(&chrome_path, &profile_dir, sandbox, request_timeout).await {
            Ok(b) => b,
            Err(e) if sandbox => {
                eprintln!(
                    "Warning: Chrome did not start with its sandbox ({e:#}); retrying without it. \
                     Set {NO_SANDBOX_ENV}=1 to skip this attempt."
                );
                launch(&chrome_path, &profile_dir, false, request_timeout).await?
            }
            Err(e) => {
                if !chosen.is_saved() {
                    let _ = std::fs::remove_dir_all(&profile_dir);
                    track_profile(&profile_dir, false);
                }
                return Err(e);
            }
        };

        let chrome_ua = browser
            .version()
            .await
            .map(|v| v.user_agent)
            .unwrap_or_default();

        // With a saved profile, sites see the same browser you signed in with
        // (sessions and "I am human" passes are often tied to the user agent)
        let user_agent = if chosen.is_saved() {
            chrome_ua.replace("HeadlessChrome", "Chrome")
        } else {
            honest_user_agent(&chrome_ua)
        };

        Ok(Self {
            browser,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            user_agent,
            profile: chosen,
            cookies: Vec::new(),
            cookie_jar: Vec::new(),
            custom_user_agent: false,
            request_timeout,
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
        DownloadHeaders {
            user_agent: self.user_agent.clone(),
            cookie: url::Url::parse(url)
                .ok()
                .and_then(|u| crate::cookies::header_for(&self.cookie_jar, &u)),
        }
    }

    /// True while Chrome still answers: one that crashed or was killed fails
    /// at once, one that hangs after the request timeout. A long-lived caller
    /// (the MCP server) then stops it and starts a new one.
    pub async fn is_alive(&self) -> bool {
        within(self.request_timeout, "version", self.browser.version())
            .await
            .is_ok()
    }

    /// Stop this pool's Chrome if it is still running and wait (up to
    /// [`CHROME_EXIT_WAIT`]) until it is gone, so a new Chrome can take its
    /// profile. Found through the profile's lock, so it works while other
    /// calls still hold the pool.
    pub async fn stop_chrome(&self) {
        let Some(pid) = profile_in_use(self.profile.path()) else {
            return;
        };
        kill_process(pid);
        wait_until(CHROME_EXIT_WAIT, || !process_alive(pid)).await;
    }

    /// Get a new page with resource blocking and Chrome's cache turned off, so
    /// every navigation asks the site again (a long-lived browser would
    /// otherwise answer from its cache).
    ///
    /// # Errors
    /// Returns an error if page creation or configuration fails.
    pub async fn new_page(&self) -> Result<BrowserPage> {
        let permit = self.semaphore.clone().acquire_owned().await?;
        let limit = self.request_timeout;
        let page = within(limit, "opening a tab", self.browser.new_page("about:blank")).await?;
        // Closed before the error is returned when setting it up fails, so a
        // long-lived browser does not collect half-made tabs
        if let Err(e) = self.set_up(&page).await {
            let _ = within(limit, "closing a tab", page.close()).await;
            return Err(e);
        }
        Ok(BrowserPage {
            page,
            permit: Some(permit),
            request_timeout: limit,
        })
    }

    async fn set_up(&self, page: &Page) -> Result<()> {
        let limit = self.request_timeout;
        let agent = chromiumoxide::cdp::browser_protocol::network::SetUserAgentOverrideParams::new(
            &self.user_agent,
        );
        within(limit, "setting the user agent", page.execute(agent)).await?;

        if !self.cookies.is_empty() {
            // Raw CDP call: chromiumoxide's helper refuses to run on about:blank
            let cookies = SetCookiesParams::new(self.cookies.clone());
            within(limit, "setting cookies", page.execute(cookies))
                .await
                .context("Failed to set cookies")?;
        }

        let no_cache = SetCacheDisabledParams::new(true);
        within(limit, "turning off the cache", page.execute(no_cache)).await?;

        // Enable lifecycle events for networkIdle detection
        let lifecycle = SetLifecycleEventsEnabledParams::new(true);
        within(limit, "enabling page events", page.execute(lifecycle)).await?;
        Ok(())
    }

    /// Close a pool shared through `Arc` once its last user is done: when other
    /// clones are still held, it is left to them (it closes when dropped).
    ///
    /// # Errors
    /// Returns an error if the browser fails to close cleanly.
    pub async fn close_shared(pool: Arc<Self>) -> Result<()> {
        match Arc::into_inner(pool) {
            Some(pool) => pool.close().await,
            None => Ok(()),
        }
    }

    /// Close the browser and remove its profile folder
    ///
    /// # Errors
    /// Returns an error if the browser fails to close cleanly.
    pub async fn close(mut self) -> Result<()> {
        let closed = within(self.request_timeout, "closing", self.browser.close()).await;
        // A Chrome that hangs does not exit when asked: kill it after a while
        if tokio::time::timeout(CHROME_EXIT_WAIT, self.browser.wait())
            .await
            .is_err()
        {
            let _ = self.browser.kill().await;
        }
        if let Profile::Fresh(dir) = &self.profile {
            let _ = std::fs::remove_dir_all(dir);
            track_profile(dir, false);
        }
        closed?;
        Ok(())
    }
}

impl Drop for BrowserPool {
    fn drop(&mut self) {
        // Chrome is killed on drop by chromiumoxide but may still write to the
        // profile while it exits; the folder stays tracked so the final
        // `remove_open_profiles` call in main removes it after that.
        if let Profile::Fresh(dir) = &self.profile {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Extra Chrome switches as (name, value) pairs, without the leading `--`
/// (chromiumoxide adds it; an empty value means a bare switch).
///
/// Most keep Chrome from calling Google (component updates, sync, sign-in,
/// safe browsing, metrics, field trials) while it renders pages for ref.
const CHROME_SWITCHES: [(&str, &str); 21] = [
    ("headless", "new"),
    ("disable-gpu", ""),
    ("disable-dev-shm-usage", ""),
    ("no-first-run", ""),
    ("no-default-browser-check", ""),
    ("disable-background-networking", ""),
    ("disable-component-update", ""),
    ("disable-sync", ""),
    ("disable-default-apps", ""),
    ("disable-extensions", ""),
    ("disable-component-extensions-with-background-pages", ""),
    ("disable-domain-reliability", ""),
    ("disable-client-side-phishing-detection", ""),
    ("disable-breakpad", ""),
    ("disable-field-trial-config", ""),
    ("metrics-recording-only", ""),
    ("no-pings", ""),
    ("no-service-autorun", ""),
    ("password-store", "basic"),
    ("use-mock-keychain", ""),
    ("safebrowsing-disable-auto-update", ""),
];

/// Chrome features to turn off. Passed under the same `disable-features` key
/// chromiumoxide uses (its default is `TranslateUI`), so its builder merges
/// both lists into one switch
/// (Chrome keeps only the last of repeated switches, and the order of
/// separately keyed switches is random).
const DISABLED_FEATURES: [&str; 16] = [
    "Translate",
    "AutofillServerCommunication",
    "NetworkTimeServiceQuerying",
    "PreconnectToSearch",
    "OptimizationHints",
    "OptimizationGuideModelDownloading",
    "MediaRouter",
    "DialMediaRouteProvider",
    "CertificateTransparencyComponentUpdater",
    "InterestFeedContentSuggestions",
    "AccountConsistency",
    "GlobalMediaControls",
    "SafeBrowsingEnhancedProtection",
    "PrivacySandboxSettings4",
    "AimEnabled",
    "WebUIOmniboxPopup",
];

/// Apply [`CHROME_SWITCHES`] and [`DISABLED_FEATURES`] to a launch config.
fn with_chrome_args(mut builder: BrowserConfigBuilder) -> BrowserConfigBuilder {
    for (key, value) in CHROME_SWITCHES {
        builder = if value.is_empty() {
            builder.arg(key)
        } else {
            builder.arg((key, value))
        };
    }
    builder.arg(("disable-features", &DISABLED_FEATURES[..]))
}

async fn launch(
    chrome_path: &Path,
    profile_dir: &Path,
    sandbox: bool,
    request_timeout: Duration,
) -> Result<Browser> {
    // chromiumoxide adds the leading "--" itself, so flags are written without it
    // (written with it, Chrome receives "----flag" and silently ignores it).
    let mut builder = BrowserConfig::builder()
        .chrome_executable(chrome_path)
        .user_data_dir(profile_dir)
        .request_timeout(request_timeout);
    builder = with_chrome_args(builder);
    if !sandbox {
        builder = builder.no_sandbox();
    }
    let config = builder
        .build()
        .map_err(|e| anyhow::anyhow!("Browser config error: {e}"))?;

    let (browser, mut handler) = Browser::launch(config)
        .await
        .context("Failed to launch Chrome")?;

    // Spawn handler in background
    // An error here means the connection to Chrome is gone (messages it cannot
    // parse are skipped, not errors). Ending the task then makes every request
    // fail at once, instead of waiting for an answer that never comes.
    tokio::spawn(async move { while let Some(Ok(())) = handler.next().await {} });
    Ok(browser)
}

/// Make sure a saved profile can be used: not Chrome's everyday folder (Chrome
/// 136+ refuses remote control there), created if missing, and not open in
/// another Chrome.
///
/// # Errors
/// Returns an error explaining what to do instead.
pub(crate) fn prepare_saved_profile(dir: &Path, home: Option<&Path>) -> Result<()> {
    if profile::is_chrome_default_dir(dir, home) {
        anyhow::bail!(
            "{} is your everyday Chrome profile. Chrome (version 136 and later) does not \
             let tools like ref control that profile, to protect its cookies. Use ref's \
             own saved profile instead: run `ref login` (without --profile), sign in to \
             your sites there once, and ref reuses those sessions.",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Cannot create the profile folder {}", dir.display()))?;
    if let Some(pid) = profile_in_use(dir) {
        anyhow::bail!(
            "The profile {} is open in another Chrome (process {pid}): a `ref login` window, \
             a running `ref mcp` server or another ref command. Quit it fully (for `ref login`, \
             close every window) and try again.",
            dir.display()
        );
    }
    Ok(())
}

/// The live process holding Chrome's `SingletonLock` in `dir`, if any
pub(crate) fn profile_in_use(dir: &Path) -> Option<u32> {
    let target = std::fs::read_link(dir.join("SingletonLock")).ok()?;
    let pid = profile::lock_owner_pid(&target.to_string_lossy())?;
    process_alive(pid).then_some(pid)
}

/// True while process `pid` runs. An exited process its parent has not
/// collected yet (a zombie) counts as gone: it holds nothing any more.
#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| !is_zombie(&stat))
}

/// True when a `/proc/<pid>/stat` line says the process is a zombie: the
/// state letter follows the command name, which is in parentheses
#[cfg(target_os = "linux")]
fn is_zombie(stat: &str) -> bool {
    stat.rsplit_once(')')
        .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
}

/// Kill process `pid` outright
#[cfg(unix)]
fn kill_process(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

#[cfg(not(unix))]
fn kill_process(_pid: u32) {}

/// Wait until `done` is true, checking every 100 ms, for at most `limit`;
/// true when it became true in time
async fn wait_until(limit: Duration, done: impl Fn() -> bool) -> bool {
    let start = std::time::Instant::now();
    while !done() {
        if start.elapsed() >= limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// Clean up after ref runs that were killed outright (SIGKILL, power loss):
/// their Chrome may still be running and their profile is still on disk.
/// A profile is stale when the ref process named in it no longer exists.
#[cfg(target_os = "linux")]
fn sweep_orphans() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = profile_owner(&name) else {
            continue;
        };
        if pid == std::process::id() || Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let dir = entry.path();
        let killed = kill_chrome_using(&dir);
        // A killed Chrome can still write to its profile while it exits
        for _ in 0..if killed { 3 } else { 1 } {
            let _ = std::fs::remove_dir_all(&dir);
            if killed {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn sweep_orphans() {}

/// Kill every process whose command line uses `profile` as its user-data-dir;
/// true when any was found
#[cfg(target_os = "linux")]
fn kill_chrome_using(profile: &Path) -> bool {
    let needle = format!("--user-data-dir={}", profile.display());
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    let mut killed = false;
    for proc_entry in procs.flatten() {
        let pid = proc_entry.file_name().to_string_lossy().into_owned();
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let cmdline = std::fs::read(proc_entry.path().join("cmdline")).unwrap_or_default();
        let uses_profile = cmdline
            .split(|b| *b == 0)
            .any(|arg| arg == needle.as_bytes());
        if uses_profile {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid])
                .status();
            killed = true;
        }
    }
    killed
}

/// The ref process id in a profile folder name `ref-chrome-<pid>-<nanos>`
fn profile_owner(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("ref-chrome-")?;
    let (pid, nanos) = rest.split_once('-')?;
    nanos.parse::<u32>().ok()?;
    pid.parse().ok()
}

/// A profile folder unique to this process and pool
fn unique_profile_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    std::env::temp_dir().join(format!("ref-chrome-{}-{nanos}", std::process::id()))
}

/// Chrome refuses its sandbox as root; users can also opt out explicitly.
pub(crate) fn want_sandbox(env_value: Option<&str>, root: bool) -> bool {
    let opted_out = env_value.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    !opted_out && !root
}

#[cfg(target_os = "linux")]
pub(crate) fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|s| uid_is_root(&s))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn running_as_root() -> bool {
    false
}

/// Parse the `Uid:` line of `/proc/self/status` (real, effective, ...).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn uid_is_root(status: &str) -> bool {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().nth(1))
        == Some("0")
}

/// Check a user agent given by the user: not empty, one line, no control characters
/// (it goes into a request header). Surrounding spaces are trimmed.
///
/// # Errors
/// Returns an error saying what is wrong with it.
pub fn validate_user_agent(user_agent: &str) -> Result<String> {
    let ua = user_agent.trim();
    if ua.is_empty() {
        anyhow::bail!("The user agent is empty. Give a name and an email, e.g. \"Jane Doe jane@example.com\".");
    }
    if ua.chars().any(char::is_control) {
        anyhow::bail!("The user agent must be one line without control characters.");
    }
    Ok(ua.to_string())
}

/// Chrome's own user agent, without the "Headless" marker hidden and with
/// ref identifying itself, so sites see a real browser version and who is asking.
fn honest_user_agent(chrome_ua: &str) -> String {
    let base = if chrome_ua.is_empty() {
        "Mozilla/5.0 (compatible)".to_string()
    } else {
        chrome_ua.replace("HeadlessChrome", "Chrome")
    };
    format!(
        "{base} mollendorff-ref/{} (+https://github.com/mollendorff-ai/ref)",
        env!("CARGO_PKG_VERSION")
    )
}

fn to_cookie_param(c: &Cookie) -> Option<CookieParam> {
    let mut b = CookieParam::builder()
        .name(c.name.clone())
        .value(c.value.clone())
        .domain(c.domain.clone())
        .path(c.path.clone())
        .secure(c.secure)
        .http_only(c.http_only);
    if let Some(exp) = c.expires {
        #[allow(clippy::cast_precision_loss)]
        let exp = exp as f64;
        b = b.expires(TimeSinceEpoch::new(exp));
    }
    b.build().ok()
}

/// Copies each open shadow root's HTML into a `<div data-ref-shadow>` inside its
/// host, innermost hosts first so nested components are included.
const FLATTEN_SHADOW_ROOTS: &str = r"(() => {
  const hosts = [];
  const roots = [document];
  while (roots.length) {
    const root = roots.shift();
    for (const el of root.querySelectorAll('*')) {
      if (el.shadowRoot) { hosts.push(el); roots.push(el.shadowRoot); }
    }
  }
  for (const host of hosts.reverse()) {
    const copy = document.createElement('div');
    copy.setAttribute('data-ref-shadow', '');
    copy.innerHTML = host.shadowRoot.innerHTML;
    host.appendChild(copy);
  }
  return hosts.length;
})()";

/// Request headers for a download made outside Chrome
#[derive(Debug, Clone, Default)]
pub struct DownloadHeaders {
    pub user_agent: String,
    pub cookie: Option<String>,
}

/// A browser tab. Dropping it closes the tab and then frees its slot in the pool,
/// so a long-running MCP server does not pile up open tabs.
pub struct BrowserPage {
    page: Page,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    /// Limit on each request to Chrome (see [`within`])
    request_timeout: Duration,
}

impl Drop for BrowserPage {
    fn drop(&mut self) {
        let (page, permit, limit) = (self.page.clone(), self.permit.take(), self.request_timeout);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // Bounded: a hung Chrome would otherwise keep this tab's slot forever
            handle.spawn(async move {
                let _ = within(limit, "closing a tab", page.close()).await;
                drop(permit);
            });
        }
    }
}

impl BrowserPage {
    /// Navigate to URL and wait for network idle (SPA support)
    ///
    /// This method:
    /// 1. Subscribes to lifecycle events
    /// 2. Navigates to the URL
    /// 3. Waits for `networkIdle` event (no requests for 500ms)
    /// 4. Falls back to timeout if `networkIdle` not reached
    ///
    /// This ensures SPAs have time to load their dynamic content.
    ///
    /// # Errors
    /// Returns an error if the lifecycle event subscription fails.
    pub async fn goto(&self, url: &str, timeout_ms: u64) -> Result<PageResult> {
        // Subscribe to lifecycle and response events BEFORE navigation
        let limit = self.request_timeout;
        let mut lifecycle = within(limit, "subscribing to page events", async {
            self.page.event_listener::<EventLifecycleEvent>().await
        })
        .await
        .context("Failed to subscribe to lifecycle events")?;
        let mut responses = within(limit, "subscribing to responses", async {
            self.page.event_listener::<EventResponseReceived>().await
        })
        .await
        .context("Failed to subscribe to network responses")?;

        // Start navigation with overall timeout
        let nav_result =
            tokio::time::timeout(Duration::from_millis(timeout_ms), self.page.goto(url)).await;

        // Handle navigation result
        match nav_result {
            Ok(Ok(_)) => {
                // Navigation succeeded, now wait for networkIdle
                self.wait_for_network_idle(&mut lifecycle).await;

                let frame = within(limit, "reading the main frame", self.page.mainframe());
                let main_frame = frame.await.ok().flatten();
                let mut documents = Vec::new();
                while let Some(Some(event)) = responses.next().now_or_never() {
                    if event.r#type == ResourceType::Document {
                        documents.push(DocumentResponse {
                            frame: event.frame_id.as_ref().map(|f| f.inner().clone()),
                            status: event.response.status,
                            mime: event.response.mime_type.clone(),
                        });
                    }
                }
                let main_frame = main_frame.as_ref().map(|f| f.inner().as_str());
                let main = main_document(&documents, main_frame);
                let status = main.and_then(|d| u16::try_from(d.status).ok());
                let mime = main.map(|d| d.mime.to_ascii_lowercase());
                let title = within(limit, "reading the title", self.page.get_title());
                let title = title.await.ok().flatten();
                Ok(PageResult {
                    status,
                    mime,
                    title,
                    error: None,
                })
            }
            Ok(Err(e)) => Ok(PageResult {
                status: None,
                mime: None,
                title: None,
                error: Some(describe_error(&e.to_string())),
            }),
            Err(_) => Ok(PageResult {
                status: None,
                mime: None,
                title: None,
                error: Some("Navigation timeout".to_string()),
            }),
        }
    }

    /// Wait for `networkIdle` lifecycle event with timeout
    ///
    /// `networkIdle` fires when there are no network requests for 500ms.
    /// This is ideal for SPAs that load content via XHR/fetch.
    ///
    /// Falls back gracefully on timeout - some sites never reach `networkIdle`
    /// due to analytics, websockets, or polling.
    async fn wait_for_network_idle(
        &self,
        lifecycle: &mut chromiumoxide::listeners::EventStream<EventLifecycleEvent>,
    ) {
        let wait_result =
            tokio::time::timeout(Duration::from_millis(NETWORK_IDLE_TIMEOUT_MS), async {
                while let Some(event) = lifecycle.next().await {
                    // networkIdle = no network connections for 500ms
                    // networkAlmostIdle = ≤2 connections for 500ms
                    if event.name == "networkIdle" {
                        return WaitResult::NetworkIdle;
                    }
                }
                WaitResult::StreamEnded
            })
            .await;

        // All outcomes are acceptable: networkIdle (ideal), stream ended, or timeout
        // Sites with persistent connections (analytics, websockets) may never reach networkIdle
        let _ = wait_result;
    }

    /// Get page content (for data extraction)
    ///
    /// Text inside open shadow roots (web components) is not part of the
    /// serialized HTML, so it is copied into the page first; closed shadow
    /// roots cannot be read.
    ///
    /// # Errors
    /// Returns an error if content extraction fails.
    pub async fn content(&self) -> Result<String> {
        // Best effort: a page without web components, or with scripts blocked,
        // is read as it is
        let limit = self.request_timeout;
        let _ = within(
            limit,
            "reading web components",
            self.page.evaluate(FLATTEN_SHADOW_ROOTS),
        )
        .await;
        within(limit, "reading the page", self.page.content())
            .await
            .context("Failed to get page content")
    }

    /// Get current URL (after redirects)
    pub async fn current_url(&self) -> Option<String> {
        within(self.request_timeout, "reading the address", self.page.url())
            .await
            .ok()
            .flatten()
    }
}

/// Internal result type for network idle wait
enum WaitResult {
    NetworkIdle,
    StreamEnded,
}

/// A `Document` response seen during navigation
struct DocumentResponse {
    frame: Option<String>,
    status: i64,
    mime: String,
}

/// The main frame's document response, or the first one seen
/// (iframes load after the main document).
/// `None` when Chrome reported no HTTP response (for example `data:` URLs).
fn main_document<'a>(
    documents: &'a [DocumentResponse],
    main_frame: Option<&str>,
) -> Option<&'a DocumentResponse> {
    main_frame
        .and_then(|id| documents.iter().find(|d| d.frame.as_deref() == Some(id)))
        .or_else(|| documents.first())
}

/// Result of a page navigation
#[derive(Debug)]
pub struct PageResult {
    /// HTTP status of the main document, when Chrome reported one
    pub status: Option<u16>,
    /// MIME type of the main document (lowercase), e.g. `text/html`, `application/pdf`
    pub mime: Option<String>,
    pub title: Option<String>,
    pub error: Option<String>,
}

/// Turn a Chrome navigation error into a short, stable label plus the raw message.
fn describe_error(error: &str) -> String {
    let label = if error.contains("ERR_NAME_NOT_RESOLVED") {
        "DNS_FAILED"
    } else if error.contains("ERR_CONNECTION_REFUSED") {
        "CONNECTION_REFUSED"
    } else if error.contains("ERR_CONNECTION_TIMED_OUT") || error.contains("ERR_TIMED_OUT") {
        "TIMEOUT"
    } else if error.contains("ERR_CERT") || error.contains("SSL") {
        "SSL_ERROR"
    } else {
        "NETWORK_ERROR"
    };
    format!("{label}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    pub(crate) const FRESH: Option<&ProfileChoice> = Some(&ProfileChoice::Fresh);

    impl BrowserPool {
        /// Kill this pool's Chrome outright, as a crash would, and wait until it is gone
        pub(crate) async fn kill_chrome(&self) {
            let pid = profile_in_use(self.profile.path()).expect("Chrome holds its profile");
            let killed = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status()
                .expect("run kill");
            assert!(killed.success());
            // A dead Chrome stops answering at once; is_alive waits up to 3 s
            while self.is_alive().await {}
        }
    }

    /// Serve one page that tells the browser to cache it for an hour, with a
    /// number that goes up on every request
    fn serve_cacheable_counter() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().flatten().enumerate() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
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
        pool.close().await.unwrap();
        // Chrome may also ask for /favicon.ico, which counts as a visit too
        assert!(seen[0].contains("visit 0"), "{}", seen[0]);
        assert!(
            !seen[1].contains("visit 0"),
            "served from the cache: {}",
            seen[1]
        );
    }

    #[tokio::test]
    async fn a_shared_pool_closes_with_its_last_user() {
        // Needs Chrome (ADR-004)
        let pool = Arc::new(BrowserPool::new(1, FRESH).await.unwrap());
        let other = Arc::clone(&pool);
        BrowserPool::close_shared(pool).await.unwrap();
        assert!(other.is_alive().await, "closed while still in use");
        let dir = other.profile.path().to_path_buf();
        BrowserPool::close_shared(other).await.unwrap();
        assert!(
            !dir.exists(),
            "the last user's close removes the fresh profile"
        );
    }

    /// Freeze this pool's Chrome (SIGSTOP), as a hang would: it stays running
    /// but answers nothing
    fn freeze_chrome(pool: &BrowserPool) -> u32 {
        let pid = profile_in_use(pool.profile.path()).expect("Chrome holds its profile");
        let frozen = std::process::Command::new("kill")
            .args(["-STOP", &pid.to_string()])
            .status()
            .unwrap();
        assert!(frozen.success());
        pid
    }

    #[tokio::test]
    async fn a_hung_chrome_is_killed_on_close() {
        // Needs Chrome (ADR-004). Before, close waited forever for it to exit.
        let pool = BrowserPool::with_request_timeout(1, FRESH, Duration::from_secs(1))
            .await
            .unwrap();
        let dir = pool.profile.path().to_path_buf();
        let pid = freeze_chrome(&pool);
        let started = std::time::Instant::now();
        let _ = pool.close().await;
        let took = started.elapsed();
        assert!(took < Duration::from_secs(20), "{took:?}");
        assert!(!process_alive(pid), "the hung Chrome still runs");
        assert!(!dir.exists(), "the fresh profile was not removed");
    }

    #[tokio::test]
    async fn stopping_chrome_frees_its_profile_even_when_it_hangs() {
        // Needs Chrome (ADR-004)
        let pool = BrowserPool::with_request_timeout(1, FRESH, Duration::from_secs(1))
            .await
            .unwrap();
        let pid = freeze_chrome(&pool);
        assert!(!pool.is_alive().await, "a frozen Chrome answered");
        pool.stop_chrome().await;
        assert!(!process_alive(pid));
        assert_eq!(profile_in_use(pool.profile.path()), None);
        // Already gone: nothing to stop
        pool.stop_chrome().await;
    }

    /// The tabs open in Chrome, as Chrome itself counts them
    async fn open_tabs(pool: &BrowserPool) -> usize {
        let targets = chromiumoxide::cdp::browser_protocol::target::GetTargetsParams::default();
        let answer = pool.browser.execute(targets).await.unwrap();
        answer
            .result
            .target_infos
            .iter()
            .filter(|t| t.r#type == "page")
            .count()
    }

    #[tokio::test]
    async fn a_tab_that_fails_to_set_up_is_closed() {
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
        let tabs_after = open_tabs(&pool).await;
        assert_eq!(tabs_after, tabs_before, "the half-made tab is still open");
        pool.close().await.unwrap();
    }

    #[tokio::test]
    async fn waiting_ends_when_done_or_at_the_limit() {
        assert!(wait_until(Duration::from_secs(1), || true).await);
        let started = std::time::Instant::now();
        assert!(!wait_until(Duration::from_millis(250), || false).await);
        assert!(started.elapsed() >= Duration::from_millis(250));
    }

    #[test]
    fn zombies_are_recognised_from_proc_stat() {
        assert!(is_zombie("4321 (chrome) Z 1 4321 4321 0 -1"));
        assert!(is_zombie("17 (odd ) name) Z 1"));
        assert!(!is_zombie("4321 (chrome) S 1 4321 4321 0 -1"));
        assert!(!is_zombie("garbage"));
        assert!(process_alive(std::process::id()));
    }

    #[tokio::test]
    async fn a_killed_chrome_is_no_longer_alive() {
        // Needs Chrome (ADR-004)
        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        assert!(pool.is_alive().await);
        pool.kill_chrome().await;
        assert!(!pool.is_alive().await);
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
        pool.close().await.unwrap();
        assert!(saved.is_dir(), "a saved profile is kept after close");

        let pool = BrowserPool::new(1, FRESH).await.unwrap();
        assert!(!pool.profile.is_saved(), "{:?}", pool.profile);
        let temp = pool.profile.path().to_path_buf();
        assert_ne!(temp, saved);
        pool.close().await.unwrap();
        assert!(!temp.exists(), "a fresh profile is removed on close");
    }

    #[test]
    fn test_describe_error() {
        assert!(describe_error("net::ERR_NAME_NOT_RESOLVED").starts_with("DNS_FAILED: "));
        assert!(describe_error("ERR_CONNECTION_REFUSED").starts_with("CONNECTION_REFUSED: "));
        assert!(describe_error("net::ERR_CERT_DATE_INVALID").starts_with("SSL_ERROR: "));
        assert!(describe_error("random error").starts_with("NETWORK_ERROR: "));
        assert!(describe_error("random error").ends_with("random error"));
    }

    fn main_document_status(docs: &[DocumentResponse], main_frame: Option<&str>) -> Option<u16> {
        main_document(docs, main_frame).and_then(|d| u16::try_from(d.status).ok())
    }

    fn doc(frame: Option<&str>, status: i64) -> DocumentResponse {
        DocumentResponse {
            frame: frame.map(String::from),
            status,
            mime: "text/html".to_string(),
        }
    }

    #[test]
    fn test_main_document_status_prefers_main_frame() {
        let docs = [doc(Some("iframe"), 200), doc(Some("main"), 404)];
        assert_eq!(main_document_status(&docs, Some("main")), Some(404));
    }

    #[test]
    fn test_main_document_status_falls_back_to_first() {
        let docs = [doc(Some("a"), 503), doc(Some("b"), 200)];
        assert_eq!(main_document_status(&docs, Some("missing")), Some(503));
        assert_eq!(main_document_status(&docs, None), Some(503));
    }

    #[test]
    fn test_main_document_status_none_without_responses() {
        assert_eq!(main_document_status(&[], Some("main")), None);
        assert_eq!(main_document_status(&[doc(None, -1)], None), None);
    }

    #[test]
    fn test_detect_chrome_path() {
        // This test verifies the function runs without panic
        // Actual detection depends on system Chrome installation
        let result = detect_chrome_path();
        // On CI without Chrome, this may be None - that's fine
        if let Some(path) = result {
            assert!(path.exists());
        }
    }

    #[test]
    fn test_chrome_switches_are_bare_names() {
        for (key, value) in CHROME_SWITCHES {
            assert!(!key.starts_with('-') && !key.contains('='), "{key}");
            assert!(!value.contains(' '), "{value}");
        }
        assert!(DISABLED_FEATURES.iter().all(|f| !f.contains([',', ' '])));
    }

    #[test]
    fn test_want_sandbox() {
        assert!(want_sandbox(None, false));
        assert!(!want_sandbox(None, true));
        assert!(!want_sandbox(Some("1"), false));
        assert!(!want_sandbox(Some("TRUE"), false));
        assert!(want_sandbox(Some("0"), false));
    }

    #[test]
    fn test_uid_is_root() {
        assert!(uid_is_root("Name:\tref\nUid:\t1000\t0\t0\t0\n"));
        assert!(!uid_is_root("Uid:\t1000\t1000\t1000\t1000\n"));
        assert!(!uid_is_root("Name:\tref\n"));
    }

    #[test]
    fn test_validate_user_agent() {
        assert_eq!(
            validate_user_agent("  Jane Doe jane@example.com ").unwrap(),
            "Jane Doe jane@example.com"
        );
        assert!(validate_user_agent("   ").is_err());
        assert!(validate_user_agent("a\r\nX-Evil: 1").is_err());
    }

    #[test]
    fn test_honest_user_agent() {
        let ua = honest_user_agent(
            "Mozilla/5.0 (X11; Linux x86_64) HeadlessChrome/142.0.0.0 Safari/537.36",
        );
        assert!(ua.contains(" Chrome/142.0.0.0 "));
        assert!(!ua.contains("Headless"));
        assert!(ua.contains("mollendorff-ref/"));
        assert!(honest_user_agent("").starts_with("Mozilla/5.0 (compatible) mollendorff-ref/"));
    }

    #[test]
    fn test_profile_owner() {
        assert_eq!(profile_owner("ref-chrome-4195-594839600"), Some(4195));
        assert_eq!(profile_owner("ref-chrome-x-1"), None);
        assert_eq!(profile_owner("ref-chrome-12"), None);
        assert_eq!(profile_owner("ref-chrome-12-abc"), None);
        assert_eq!(profile_owner("chromiumoxide-runner"), None);
        let own = unique_profile_dir();
        let name = own.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(profile_owner(&name), Some(std::process::id()));
    }

    #[test]
    fn test_unique_profile_dirs() {
        let a = unique_profile_dir();
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert_ne!(a, unique_profile_dir());
        assert!(a.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn test_to_cookie_param() {
        let c = Cookie {
            domain: ".example.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            expires: Some(1_893_456_000),
            name: "sid".into(),
            value: "abc".into(),
        };
        let p = to_cookie_param(&c).unwrap();
        assert_eq!(p.name, "sid");
        assert_eq!(p.domain.as_deref(), Some(".example.com"));
        assert!(p.expires.is_some());
        let session = Cookie { expires: None, ..c };
        assert!(to_cookie_param(&session).unwrap().expires.is_none());
    }

    #[test]
    fn test_network_idle_timeout_reasonable() {
        let ms = NETWORK_IDLE_TIMEOUT_MS;
        assert!(ms >= 5000, "Timeout too short for SPAs");
        assert!(ms <= 30000, "Timeout too long, will slow down all fetches");
    }

    #[test]
    fn test_wait_result_variants() {
        // Ensure WaitResult enum is constructible for all variants
        let idle = WaitResult::NetworkIdle;
        let ended = WaitResult::StreamEnded;
        assert!(matches!(idle, WaitResult::NetworkIdle));
        assert!(matches!(ended, WaitResult::StreamEnded));
    }
}
