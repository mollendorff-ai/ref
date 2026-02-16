//! Headless Chrome browser management via chromiumoxide
//!
//! v1.2.0: Added `networkIdle` wait for SPA support (ADR-002)

use anyhow::{Context, Result};
use chromiumoxide::cdp::browser_protocol::page::{
    EventLifecycleEvent, SetLifecycleEventsEnabledParams,
};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
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
        // Subscribe to lifecycle events BEFORE navigation
        let mut lifecycle = self
            .page
            .event_listener::<EventLifecycleEvent>()
            .await
            .context("Failed to subscribe to lifecycle events")?;

        // Start navigation with overall timeout
        let nav_result =
            tokio::time::timeout(Duration::from_millis(timeout_ms), self.page.goto(url)).await;

        // Handle navigation result
        match nav_result {
            Ok(Ok(_)) => {
                // Navigation succeeded, now wait for networkIdle
                self.wait_for_network_idle(&mut lifecycle).await;

                let status = self.get_status().await;
                let title = self.page.get_title().await.ok().flatten();
                Ok(PageResult {
                    status,
                    title,
                    error: None,
                })
            }
            Ok(Err(e)) => {
                let (status, _) = parse_error(&e.to_string());
                Ok(PageResult {
                    status,
                    title: None,
                    error: Some(e.to_string()),
                })
            }
            Err(_) => Ok(PageResult {
                status: 0,
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

    /// Try to get HTTP status from the page (heuristic based on page content)
    async fn get_status(&self) -> u16 {
        // chromiumoxide doesn't expose HTTP status directly
        // We check if page loaded successfully by looking for error pages
        if let Ok(Some(t)) = self.page.get_title().await {
            let t_lower = t.to_lowercase();
            if t_lower.contains("404") || t_lower.contains("not found") {
                return 404;
            }
            if t_lower.contains("403")
                || t_lower.contains("forbidden")
                || t_lower.contains("access denied")
            {
                return 403;
            }
            if t_lower.contains("500") || t_lower.contains("internal server error") {
                return 500;
            }
        }
        // If we got here, assume success
        200
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

/// Result of a page navigation
#[derive(Debug)]
pub struct PageResult {
    pub status: u16,
    pub title: Option<String>,
    pub error: Option<String>,
}

fn parse_error(error: &str) -> (u16, String) {
    if error.contains("ERR_NAME_NOT_RESOLVED") {
        (0, "DNS_FAILED".to_string())
    } else if error.contains("ERR_CONNECTION_REFUSED") {
        (0, "CONNECTION_REFUSED".to_string())
    } else if error.contains("ERR_CONNECTION_TIMED_OUT") {
        (0, "TIMEOUT".to_string())
    } else if error.contains("ERR_CERT") || error.contains("SSL") {
        (0, "SSL_ERROR".to_string())
    } else {
        (0, "NETWORK_ERROR".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_error() {
        assert_eq!(parse_error("net::ERR_NAME_NOT_RESOLVED").1, "DNS_FAILED");
        assert_eq!(
            parse_error("ERR_CONNECTION_REFUSED").1,
            "CONNECTION_REFUSED"
        );
        assert_eq!(parse_error("random error").1, "NETWORK_ERROR");
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
        // Ensure timeout is reasonable (not too short, not too long)
        assert!(
            NETWORK_IDLE_TIMEOUT_MS >= 5000,
            "Timeout too short for SPAs"
        );
        assert!(
            NETWORK_IDLE_TIMEOUT_MS <= 30000,
            "Timeout too long, will slow down all fetches"
        );
    }

    #[test]
    fn test_wait_result_variants() {
        // Ensure WaitResult enum exists and is exhaustive
        let _idle = WaitResult::NetworkIdle;
        let _ended = WaitResult::StreamEnded;
    }
}
