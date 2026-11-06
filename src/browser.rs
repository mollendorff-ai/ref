//! Headless Chrome browser management via chromiumoxide
//!
//! v1.2.0: Added `networkIdle` wait for SPA support (ADR-002)

use crate::cookies::Cookie;
use anyhow::{Context, Result};
use chromiumoxide::cdp::browser_protocol::network::{
    CookieParam, EventResponseReceived, ResourceType, SetCookiesParams, TimeSinceEpoch,
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
fn detect_chrome_path() -> Option<PathBuf> {
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

/// Set to `1` or `true` to start Chrome without its sandbox
/// (needed in some containers; ref also falls back automatically).
pub const NO_SANDBOX_ENV: &str = "REF_CHROME_NO_SANDBOX";

/// Browser pool configuration
pub struct BrowserPool {
    browser: Browser,
    semaphore: Arc<Semaphore>,
    user_agent: String,
    profile_dir: PathBuf,
    cookies: Vec<CookieParam>,
}

impl BrowserPool {
    /// Create a new browser pool with concurrency limit
    ///
    /// Each pool gets its own Chrome profile folder, removed on [`close`](Self::close),
    /// so two ref processes never share or lock one profile.
    /// Chrome's sandbox is on unless ref runs as root, [`NO_SANDBOX_ENV`] is set,
    /// or Chrome cannot start with it (then ref retries once without it and says so).
    ///
    /// # Errors
    /// Returns an error if Chrome/Chromium is not found or fails to launch.
    pub async fn new(concurrency: usize) -> Result<Self> {
        let chrome_path = detect_chrome_path().ok_or_else(|| {
            anyhow::anyhow!(
                "Chrome/Chromium not found. Searched paths:\n  \
                 Linux: /usr/bin/google-chrome-stable, /usr/bin/google-chrome, /usr/bin/chromium-browser, /usr/bin/chromium\n  \
                 macOS: /Applications/Google Chrome.app/...\n  \
                 Windows: C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe\n\
                 Please install Chrome or Chromium."
            )
        })?;

        let profile_dir = unique_profile_dir();
        let sandbox = want_sandbox(
            std::env::var(NO_SANDBOX_ENV).ok().as_deref(),
            running_as_root(),
        );

        let browser = match launch(&chrome_path, &profile_dir, sandbox).await {
            Ok(b) => b,
            Err(e) if sandbox => {
                eprintln!(
                    "Warning: Chrome did not start with its sandbox ({e:#}); retrying without it. \
                     Set {NO_SANDBOX_ENV}=1 to skip this attempt."
                );
                launch(&chrome_path, &profile_dir, false).await?
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&profile_dir);
                return Err(e);
            }
        };

        let chrome_ua = browser
            .version()
            .await
            .map(|v| v.user_agent)
            .unwrap_or_default();

        Ok(Self {
            browser,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            user_agent: honest_user_agent(&chrome_ua),
            profile_dir,
            cookies: Vec::new(),
        })
    }

    /// Send these cookies with every page opened from now on.
    pub fn set_cookies(&mut self, cookies: &[Cookie]) {
        self.cookies = cookies.iter().filter_map(to_cookie_param).collect();
    }

    /// Get a new page with resource blocking
    ///
    /// # Errors
    /// Returns an error if page creation or configuration fails.
    pub async fn new_page(&self) -> Result<BrowserPage> {
        let permit = self.semaphore.clone().acquire_owned().await?;
        let page = self.browser.new_page("about:blank").await?;

        // Set user agent
        page.execute(
            chromiumoxide::cdp::browser_protocol::network::SetUserAgentOverrideParams::new(
                &self.user_agent,
            ),
        )
        .await?;

        if !self.cookies.is_empty() {
            // Raw CDP call: chromiumoxide's helper refuses to run on about:blank
            page.execute(SetCookiesParams::new(self.cookies.clone()))
                .await
                .context("Failed to set cookies")?;
        }

        // Enable lifecycle events for networkIdle detection
        page.execute(SetLifecycleEventsEnabledParams::new(true))
            .await?;

        Ok(BrowserPage {
            page,
            _permit: permit,
        })
    }

    /// Close the browser and remove its profile folder
    ///
    /// # Errors
    /// Returns an error if the browser fails to close cleanly.
    pub async fn close(mut self) -> Result<()> {
        let closed = self.browser.close().await;
        let _ = self.browser.wait().await;
        let _ = std::fs::remove_dir_all(&self.profile_dir);
        closed?;
        Ok(())
    }
}

impl Drop for BrowserPool {
    fn drop(&mut self) {
        // Chrome is killed on drop by chromiumoxide; remove what we can of the profile.
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

async fn launch(chrome_path: &Path, profile_dir: &Path, sandbox: bool) -> Result<Browser> {
    let mut builder = BrowserConfig::builder()
        .chrome_executable(chrome_path)
        .user_data_dir(profile_dir)
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .arg("--no-first-run")
        // No calls home to Google (updates, sync, sign-in) from a scraping browser
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-sync")
        .arg("--disable-default-apps")
        .arg("--headless=new");
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
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    Ok(browser)
}

/// A profile folder unique to this process and pool
fn unique_profile_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    std::env::temp_dir().join(format!("ref-chrome-{}-{nanos}", std::process::id()))
}

/// Chrome refuses its sandbox as root; users can also opt out explicitly.
fn want_sandbox(env_value: Option<&str>, root: bool) -> bool {
    let opted_out = env_value.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    !opted_out && !root
}

#[cfg(target_os = "linux")]
fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .is_some_and(|s| uid_is_root(&s))
}

#[cfg(not(target_os = "linux"))]
fn running_as_root() -> bool {
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

/// A browser page with automatic permit release
pub struct BrowserPage {
    page: Page,
    _permit: tokio::sync::OwnedSemaphorePermit,
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
        let mut lifecycle = self
            .page
            .event_listener::<EventLifecycleEvent>()
            .await
            .context("Failed to subscribe to lifecycle events")?;
        let mut responses = self
            .page
            .event_listener::<EventResponseReceived>()
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

                let main_frame = self.page.mainframe().await.ok().flatten();
                let mut documents = Vec::new();
                while let Some(Some(event)) = responses.next().now_or_never() {
                    if event.r#type == ResourceType::Document {
                        documents.push(DocumentResponse {
                            frame: event.frame_id.as_ref().map(|f| f.inner().clone()),
                            status: event.response.status,
                        });
                    }
                }
                let main_frame = main_frame.as_ref().map(|f| f.inner().as_str());
                let status = main_document_status(&documents, main_frame);
                let title = self.page.get_title().await.ok().flatten();
                Ok(PageResult {
                    status,
                    title,
                    error: None,
                })
            }
            Ok(Err(e)) => Ok(PageResult {
                status: None,
                title: None,
                error: Some(describe_error(&e.to_string())),
            }),
            Err(_) => Ok(PageResult {
                status: None,
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
    /// # Errors
    /// Returns an error if content extraction fails.
    pub async fn content(&self) -> Result<String> {
        self.page
            .content()
            .await
            .context("Failed to get page content")
    }

    /// Get current URL (after redirects)
    pub async fn current_url(&self) -> Option<String> {
        self.page.url().await.ok().flatten()
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
}

/// Pick the HTTP status of the main document.
///
/// Prefers the response for the main frame; falls back to the first document
/// response (iframes load after the main document).
/// Returns `None` when Chrome reported no HTTP response (for example `data:` URLs).
fn main_document_status(documents: &[DocumentResponse], main_frame: Option<&str>) -> Option<u16> {
    let main = main_frame
        .and_then(|id| documents.iter().find(|d| d.frame.as_deref() == Some(id)))
        .or_else(|| documents.first())?;
    u16::try_from(main.status).ok()
}

/// Result of a page navigation
#[derive(Debug)]
pub struct PageResult {
    /// HTTP status of the main document, when Chrome reported one
    pub status: Option<u16>,
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

    #[test]
    fn test_describe_error() {
        assert!(describe_error("net::ERR_NAME_NOT_RESOLVED").starts_with("DNS_FAILED: "));
        assert!(describe_error("ERR_CONNECTION_REFUSED").starts_with("CONNECTION_REFUSED: "));
        assert!(describe_error("net::ERR_CERT_DATE_INVALID").starts_with("SSL_ERROR: "));
        assert!(describe_error("random error").starts_with("NETWORK_ERROR: "));
        assert!(describe_error("random error").ends_with("random error"));
    }

    fn doc(frame: Option<&str>, status: i64) -> DocumentResponse {
        DocumentResponse {
            frame: frame.map(String::from),
            status,
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
