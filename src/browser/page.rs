//! A browser tab: navigation, waiting for the network to go quiet, and reading the page

use super::{page_failure, Chrome, ChromeFailed, CLOSE_ACK_LIMIT};
use anyhow::{Context, Result};
use chromiumoxide::cdp::browser_protocol::network::{
    Cookie, EventResponseReceived, GetCookiesParams, ResourceType,
};
use chromiumoxide::cdp::browser_protocol::page::EventLifecycleEvent;
use chromiumoxide::Page;
use futures::{FutureExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;

/// Default wait time for network idle (ms) - how long to wait for `networkIdle` event
const NETWORK_IDLE_TIMEOUT_MS: u64 = 10000;

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

/// A browser tab. Dropping it closes the tab and then frees its slot in the pool,
/// so a long-running MCP server does not pile up open tabs.
pub struct BrowserPage {
    pub(super) page: Page,
    pub(super) permit: Option<tokio::sync::OwnedSemaphorePermit>,
    pub(super) chrome: Arc<Chrome>,
    /// The one budget for everything read after navigation (rule 6), set
    /// when navigation ends
    pub(super) reads_until: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl Drop for BrowserPage {
    fn drop(&mut self) {
        let (page, permit) = (self.page.clone(), self.permit.take());
        // Bounded: a hung Chrome would otherwise keep this tab's slot forever
        let close = async move {
            let _ = tokio::time::timeout(CLOSE_ACK_LIMIT, page.close()).await;
            drop(permit);
        };
        // Outside a runtime the tab cannot be closed; its slot is freed
        let _ = tokio::runtime::Handle::try_current().map(|handle| handle.spawn(close));
    }
}

impl BrowserPage {
    /// The deadline for reading this page: the read budget of the last
    /// navigation, else a fresh one
    fn read_deadline(&self) -> tokio::time::Instant {
        let mut until = self.reads_until();
        *until.get_or_insert_with(|| tokio::time::Instant::now() + self.chrome.request_timeout)
    }

    /// Start the read budget of a navigation that just ended: a second
    /// navigation on the same tab (a retry) gets a budget of its own
    fn start_reads(&self) -> tokio::time::Instant {
        let until = tokio::time::Instant::now() + self.chrome.request_timeout;
        *self.reads_until() = Some(until);
        until
    }

    fn reads_until(&self) -> std::sync::MutexGuard<'_, Option<tokio::time::Instant>> {
        self.reads_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Navigate to URL and wait for network idle (SPA support)
    ///
    /// This method:
    /// 1. Subscribes to lifecycle events
    /// 2. Navigates to the URL
    /// 3. Waits for `networkIdle` event (no requests for 500ms)
    /// 4. Falls back to timeout if `networkIdle` not reached
    ///
    /// This ensures SPAs have time to load their dynamic content.
    /// A navigation that fails or times out is the page's failure (in
    /// `PageResult::error`) while Chrome still answers.
    ///
    /// # Errors
    /// Returns [`ChromeFailed`] when Chrome failed, or an error if the event
    /// subscription fails.
    pub async fn goto(&self, url: &str, timeout_ms: u64) -> Result<PageResult> {
        // The budget of an earlier navigation is spent: forget it, so reads
        // after this one (also a failed one) get a budget of their own
        *self.reads_until() = None;
        // Subscribe to lifecycle and response events BEFORE navigation
        let until = tokio::time::Instant::now() + self.chrome.request_timeout;
        let mut lifecycle = self
            .chrome
            .ask(until, "subscribing to page events", async {
                self.page.event_listener::<EventLifecycleEvent>().await
            })
            .await
            .context("Failed to subscribe to lifecycle events")?;
        let mut responses = self
            .chrome
            .ask(until, "subscribing to responses", async {
                self.page.event_listener::<EventResponseReceived>().await
            })
            .await
            .context("Failed to subscribe to network responses")?;

        // Start navigation with overall timeout
        let nav_result =
            tokio::time::timeout(Duration::from_millis(timeout_ms), self.page.goto(url)).await;

        let error = match nav_result {
            Ok(Ok(_)) => None,
            Ok(Err(e)) => Some(describe_error(&e.to_string())),
            Err(_) => Some("Navigation timeout".to_string()),
        };
        if let Some(error) = error {
            // Rule 3: a Chrome that no longer answers is not the site's fault
            let error = self
                .chrome
                .judge(anyhow::anyhow!(error), "navigating")
                .await;
            if error.is::<ChromeFailed>() {
                return Err(error);
            }
            return Ok(PageResult::failed(error.to_string()));
        }

        // Navigation succeeded, now wait for networkIdle
        self.wait_for_network_idle(&mut lifecycle).await;

        let reads = self.start_reads();
        let frame = self
            .chrome
            .ask(reads, "reading the main frame", self.page.mainframe());
        let main_frame = page_failure(frame.await)?.ok().flatten();
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
        let title = self
            .chrome
            .ask(reads, "reading the title", self.page.get_title());
        let title = page_failure(title.await)?.ok().flatten();
        Ok(PageResult {
            status,
            mime,
            title,
            error: None,
        })
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
        // networkIdle: no network connections for 500 ms. Every end is fine:
        // idle (ideal), the events ending, or the limit (sites with analytics,
        // websockets or polling may never be idle)
        let idle = lifecycle.any(|event| std::future::ready(event.name == "networkIdle"));
        let _ = tokio::time::timeout(Duration::from_millis(NETWORK_IDLE_TIMEOUT_MS), idle).await;
    }

    /// Get page content (for data extraction)
    ///
    /// Text inside open shadow roots (web components) is not part of the
    /// serialized HTML, so it is copied into the page first; closed shadow
    /// roots cannot be read.
    ///
    /// # Errors
    /// Returns [`ChromeFailed`] when Chrome failed, or an error if content
    /// extraction fails.
    pub async fn content(&self) -> Result<String> {
        let reads = self.read_deadline();
        // Best effort: a page without web components, or with scripts blocked,
        // is read as it is
        let flatten = self.page.evaluate(FLATTEN_SHADOW_ROOTS);
        let _ = self
            .chrome
            .ask(reads, "reading web components", flatten)
            .await;
        self.chrome
            .ask(reads, "reading the page", self.page.content())
            .await
            .context("Failed to get page content")
    }

    /// The `Cookie` header Chrome would send to `url`: the saved profile's
    /// sessions and any `--cookies` file. `None` when Chrome has none for it,
    /// or cannot say (the page's failure, not Chrome's).
    ///
    /// # Errors
    /// Returns [`ChromeFailed`] when Chrome failed.
    pub async fn cookie_header(&self, url: &str) -> Result<Option<String>> {
        let request = self
            .page
            .execute(GetCookiesParams::builder().url(url).build());
        let answer = self
            .chrome
            .ask(self.read_deadline(), "reading cookies", request);
        let cookies = page_failure(answer.await)?.ok();
        Ok(cookies.and_then(|answer| cookie_line(&answer.result.cookies)))
    }

    /// Get current URL (after redirects); `None` when Chrome does not say
    ///
    /// # Errors
    /// Returns [`ChromeFailed`] when Chrome failed.
    pub async fn current_url(&self) -> Result<Option<String>> {
        let address = self
            .chrome
            .ask(self.read_deadline(), "reading the address", self.page.url());
        Ok(page_failure(address.await)?.ok().flatten())
    }
}

/// Cookies as one `Cookie` header value (`a=1; b=2`); `None` when there are none
fn cookie_line(cookies: &[Cookie]) -> Option<String> {
    let pairs: Vec<String> = cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect();
    (!pairs.is_empty()).then(|| pairs.join("; "))
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

impl PageResult {
    /// A navigation that failed with `error`: no status, type or title
    #[must_use]
    pub fn failed(error: String) -> Self {
        Self {
            status: None,
            mime: None,
            title: None,
            error: Some(error),
        }
    }
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
    fn a_failed_navigation_has_only_its_error() {
        let failed = PageResult::failed("TIMEOUT: slow".into());
        assert_eq!(failed.error.as_deref(), Some("TIMEOUT: slow"));
        assert!(failed.status.is_none() && failed.mime.is_none() && failed.title.is_none());
    }

    #[tokio::test]
    async fn a_site_that_never_answers_is_a_navigation_timeout() {
        // Needs Chrome (ADR-004). Connections wait in the listener's queue,
        // never read: the request gets no answer.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let pool = crate::browser::BrowserPool::new(1, crate::browser::tests::FRESH)
            .await
            .unwrap();
        let page = pool.new_page().await.unwrap();
        let started = std::time::Instant::now();
        let result = page.goto(&url, 1_000).await.unwrap();
        assert_eq!(result.error.as_deref(), Some("Navigation timeout"));
        assert!(started.elapsed() < Duration::from_secs(10));
        // The site was slow, not Chrome
        assert!(pool.is_alive().await);
        drop((page, listener));
        pool.stop().await;
    }

    #[test]
    fn test_describe_error() {
        assert!(describe_error("net::ERR_NAME_NOT_RESOLVED").starts_with("DNS_FAILED: "));
        assert!(describe_error("ERR_CONNECTION_REFUSED").starts_with("CONNECTION_REFUSED: "));
        assert!(describe_error("net::ERR_TIMED_OUT").starts_with("TIMEOUT: "));
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
    fn test_network_idle_timeout_reasonable() {
        let ms = NETWORK_IDLE_TIMEOUT_MS;
        assert!(ms >= 5000, "Timeout too short for SPAs");
        assert!(ms <= 30000, "Timeout too long, will slow down all fetches");
    }
}
