//! refresh-data command: pull amounts and percentages from live pages
//!
//! LLM-optimized output - JSON compact only.
//! Numbers are taken from the page's main content (the same content `fetch`
//! returns), so ads and navigation do not leak in.

use crate::browser::{page_failure, BrowserPage, BrowserPool};
use crate::content;
use crate::extract::{extract_amounts, extract_percentages, extract_urls, AmountMatch};
use crate::fetch::extract_main_content;
use crate::text::truncate;
use anyhow::{Context, Result};
use clap::Args;
use scraper::Html;
use serde::Serialize;
use std::sync::Arc;
use tokio::fs;

#[derive(Args)]
pub struct RefreshDataArgs {
    /// Extract data from a single URL
    #[arg(long)]
    url: Option<String>,

    /// Markdown file to process (extract URLs and refresh data)
    #[arg(value_name = "FILE")]
    file: Option<String>,

    /// Timeout per URL in milliseconds
    #[arg(long, default_value = "20000")]
    timeout: u64,

    #[command(flatten)]
    profile: crate::profile::ProfileArgs,
}

/// Configuration for refresh-data
pub struct RefreshConfig {
    pub timeout_ms: u64,
    /// `--profile` value, or `None` for the default
    pub profile: Option<crate::profile::ProfileChoice>,
}

/// Extracted data from a URL (compact)
#[derive(Debug, Serialize)]
pub struct ExtractedData {
    pub url: String,
    pub success: bool,
    /// HTTP status of the page, when Chrome reported one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amounts: Option<Vec<AmountMatch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentages: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Report containing all extractions (compact)
#[derive(Debug, Default, Serialize)]
pub struct RefreshReport {
    pub ok: usize,
    pub failed: usize,
    pub results: Vec<ExtractedData>,
}

/// Run the refresh-data command
///
/// Returns `Ok(false)` when any URL failed.
///
/// # Errors
/// Returns an error if URLs cannot be resolved, the browser fails to launch, or JSON serialization fails.
pub async fn run_refresh_data(args: RefreshDataArgs, out: &mut dyn std::io::Write) -> Result<bool> {
    let urls = get_urls(&args).await?;

    if urls.is_empty() {
        eprintln!("No URLs found.");
        return Ok(false);
    }

    eprintln!("Extracting data from {} URLs...", urls.len());

    let config = RefreshConfig {
        timeout_ms: args.timeout,
        profile: args.profile.choice()?,
    };

    let report = refresh_data(&urls, &config).await?;

    // Output compact JSON to stdout
    writeln!(out, "{}", serde_json::to_string(&report)?)?;

    eprintln!("Done: {}/{} OK", report.ok, report.ok + report.failed);

    Ok(report.failed == 0)
}

/// Get URLs to extract from
async fn get_urls(args: &RefreshDataArgs) -> Result<Vec<String>> {
    if let Some(url) = &args.url {
        return Ok(vec![url.clone()]);
    }

    if let Some(file) = &args.file {
        let content = fs::read_to_string(file)
            .await
            .with_context(|| format!("Failed to read file: {file}"))?;
        return Ok(extract_urls(&content));
    }

    anyhow::bail!(
        "Usage:\n  ref refresh-data --url <URL>  Extract from single URL\n  \
         ref refresh-data <file.md>    Extract from all URLs in file"
    )
}

/// Extract data from multiple URLs in a browser of their own, one at a time
/// with a one-second pause
///
/// # Errors
/// Returns an error if the browser pool cannot be created, or
/// [`ChromeFailed`](crate::browser::ChromeFailed) when Chrome fails.
pub async fn refresh_data(urls: &[String], config: &RefreshConfig) -> Result<RefreshReport> {
    let pool = Arc::new(BrowserPool::new(1, config.profile.as_ref()).await?);
    let report = refresh_data_with(&pool, urls, config.timeout_ms).await;
    pool.stop().await;
    report
}

/// Extract data from multiple URLs in a browser the caller already has, such
/// as the MCP server's shared one, one at a time with a one-second pause, each
/// on a task of its own so a panic on one URL becomes that URL's failure
///
/// # Errors
/// Returns [`ChromeFailed`](crate::browser::ChromeFailed) when Chrome fails,
/// and stops then.
pub async fn refresh_data_with(
    pool: &Arc<BrowserPool>,
    urls: &[String],
    timeout_ms: u64,
) -> Result<RefreshReport> {
    let mut results = Vec::with_capacity(urls.len());

    for (i, url) in urls.iter().enumerate() {
        eprintln!("  -> {}", truncate(url, 60));
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let (pool, owned) = (Arc::clone(pool), url.clone());
        let result = crate::task::isolated(async move {
            match page_failure(pool.new_page().await)? {
                Ok(page) => extract_from_page(&page, &owned, timeout_ms).await,
                Err(e) => Ok(failure(&owned, None, format!("Browser error: {e}"))),
            }
        })
        .await;
        let internal = |e| Ok(failure(url, None, format!("Internal error in ref: {e}")));
        results.push(result.unwrap_or_else(internal)?);
    }

    let ok = results.iter().filter(|r| r.success).count();
    Ok(RefreshReport {
        ok,
        failed: results.len() - ok,
        results,
    })
}

fn failure(url: &str, http: Option<u16>, error: String) -> ExtractedData {
    ExtractedData {
        url: url.to_string(),
        success: false,
        http,
        title: None,
        amounts: None,
        percentages: None,
        error: Some(error),
    }
}

/// Extract data from a single page; navigation errors and 4xx/5xx are
/// failures of the page, and `Err` is a Chrome failure
async fn extract_from_page(
    page: &BrowserPage,
    url: &str,
    timeout_ms: u64,
) -> Result<ExtractedData> {
    if let Err(e) = crate::guard::check_web_url(url) {
        return Ok(failure(url, None, e));
    }
    let nav = match page_failure(page.goto(url, timeout_ms).await)? {
        Ok(nav) => nav,
        Err(e) => return Ok(failure(url, None, e)),
    };
    if let Some(error) = nav.error {
        return Ok(failure(url, nav.status, error));
    }
    if let Some(code) = nav.status.filter(|c| *c >= 400) {
        return Ok(failure(url, Some(code), format!("HTTP {code}")));
    }
    Ok(match page_failure(page.content().await)? {
        Ok(html) => extract_numbers(url, nav.status, &html),
        Err(e) => failure(url, nav.status, e),
    })
}

/// Amounts and percentages from the main content of a rendered page
fn extract_numbers(url: &str, http: Option<u16>, html: &str) -> ExtractedData {
    let doc = Html::parse_document(html);
    let title = crate::fetch::page_title(&doc);
    let main = Html::parse_document(&extract_main_content(&doc));
    let text = content::extract_sections(&main, 0, usize::MAX, false)
        .sections
        .iter()
        .map(|s| format!("{}\n{}", s.heading, s.content))
        .collect::<Vec<_>>()
        .join("\n");
    let amounts = extract_amounts(&text);
    let percentages = extract_percentages(&text);

    ExtractedData {
        url: url.to_string(),
        success: true,
        http,
        title,
        amounts: (!amounts.is_empty()).then_some(amounts),
        percentages: (!percentages.is_empty()).then_some(percentages),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_tab_chrome_refuses_is_that_urls_failure() {
        // Needs Chrome (ADR-004)
        let pool = crate::browser::tests::pool_that_cannot_set_up_tabs().await;
        let report = refresh_data_with(&pool, &["http://127.0.0.1:9/".to_string()], 5000).await;
        let error = report.unwrap().results[0].error.clone().unwrap_or_default();
        assert!(error.starts_with("Browser error"), "{error}");
        pool.stop().await;
    }

    #[tokio::test]
    async fn the_command_writes_its_report() {
        // Needs Chrome (ADR-004)
        let args = RefreshDataArgs {
            url: Some("http://127.0.0.1:9/".into()),
            file: None,
            timeout: 5000,
            profile: crate::profile::ProfileArgs {
                profile: Some(crate::profile::ProfileChoice::Fresh),
            },
        };
        let mut out = Vec::new();
        assert!(!run_refresh_data(args, &mut out).await.unwrap());
        let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(report["failed"], 1);
    }

    #[tokio::test]
    async fn a_chrome_failure_ends_the_run() {
        // Needs Chrome (ADR-004). Before, it was reported as that URL's failure.
        let pool = Arc::new(
            BrowserPool::new(1, Some(&crate::profile::ProfileChoice::Fresh))
                .await
                .unwrap(),
        );
        let trap = crate::browser::tests::serve_trap(&pool, "-KILL").await;
        let urls = [trap, "http://127.0.0.1:9/".to_string()];
        let err = refresh_data_with(&pool, &urls, 10_000).await.unwrap_err();
        assert!(err.is::<crate::browser::ChromeFailed>(), "{err:#}");
        pool.stop().await;
    }

    #[tokio::test]
    async fn refresh_starts_its_own_browser() {
        // Needs Chrome (ADR-004)
        let config = RefreshConfig {
            timeout_ms: 5000,
            profile: Some(crate::profile::ProfileChoice::Fresh),
        };
        let urls = ["http://127.0.0.1:9/".to_string()];
        let report = refresh_data(&urls, &config).await.unwrap();
        assert_eq!(report.failed, 1);
        assert!(!report.results[0].success);
    }

    #[test]
    fn test_numbers_come_from_main_content_only() {
        let html = "<html><head><title>Survey</title></head><body>\
                    <div class='ad'>BUY NOW 50% OFF</div>\
                    <article><h1>Survey</h1><p>About 41% use it at work; the Pro plan costs $12.</p></article>\
                    </body></html>";
        let data = extract_numbers("https://x.test", Some(200), html);
        assert!(data.success);
        assert_eq!(data.title.as_deref(), Some("Survey"));
        assert_eq!(data.percentages, Some(vec!["41%".to_string()]));
        assert_eq!(data.amounts.map(|a| a.len()), Some(1));
    }

    #[test]
    fn test_failure_shape() {
        let f = failure("https://x.test", Some(404), "HTTP 404".into());
        assert!(!f.success);
        assert_eq!(f.http, Some(404));
        assert_eq!(f.error.as_deref(), Some("HTTP 404"));
    }
}
