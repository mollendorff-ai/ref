//! Headless Chrome browser management via chromiumoxide
//!
//! v1.2.0: Added `networkIdle` wait for SPA support (ADR-002)

use anyhow::{Context, Result};
use chromiumoxide::cdp::browser_protocol::network::{EventResponseReceived, ResourceType};
use chromiumoxide::cdp::browser_protocol::page::{
    EventLifecycleEvent, SetLifecycleEventsEnabledParams,
};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::{FutureExt, StreamExt};
use std::path::PathBuf;
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

/// Browser pool configuration
pub struct BrowserPool {
    browser: Browser,
    semaphore: Arc<Semaphore>,
    user_agent: String,
}

impl BrowserPool {
    /// Create a new browser pool with concurrency limit
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

        let config = BrowserConfig::builder()
            .chrome_executable(chrome_path)
            .no_sandbox()
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-setuid-sandbox")
            .arg("--no-first-run")
            .arg("--headless=new")
            .build()
            .map_err(|e| anyhow::anyhow!("Browser config error: {e}"))?;

        let (browser, mut handler) = Browser::launch(config)
            .await
            .context("Failed to launch Chrome")?;

        // Spawn handler in background
        tokio::spawn(async move { while handler.next().await.is_some() {} });

        Ok(Self {
            browser,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_string(),
        })
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

        // Enable lifecycle events for networkIdle detection
        page.execute(SetLifecycleEventsEnabledParams::new(true))
            .await?;

        Ok(BrowserPage {
            page,
            _permit: permit,
        })
    }

    /// Close the browser
    ///
    /// # Errors
    /// Returns an error if the browser fails to close cleanly.
    pub async fn close(mut self) -> Result<()> {
        self.browser.close().await?;
        Ok(())
    }
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
