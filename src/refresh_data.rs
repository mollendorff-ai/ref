//! refresh-data command: pull amounts and percentages from live pages
//!
//! LLM-optimized output - JSON compact only.
//! Numbers are taken from the page's main content (the same content `fetch`
//! returns), so ads and navigation do not leak in.

use crate::browser::{BrowserPage, BrowserPool};
use crate::content;
use crate::extract::{extract_amounts, extract_percentages, extract_urls, AmountMatch};
use crate::fetch::extract_main_content;
use crate::text::truncate;
use anyhow::{Context, Result};
use clap::Args;
use scraper::Html;
use serde::Serialize;
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
}

/// Configuration for refresh-data
pub struct RefreshConfig {
    pub timeout_ms: u64,
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
#[derive(Debug, Serialize)]
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
pub async fn run_refresh_data(args: RefreshDataArgs) -> Result<bool> {
    let urls = get_urls(&args).await?;

    if urls.is_empty() {
        eprintln!("No URLs found.");
        std::process::exit(1);
    }

    eprintln!("Extracting data from {} URLs...", urls.len());

    let config = RefreshConfig {
        timeout_ms: args.timeout,
    };

    let report = refresh_data(&urls, &config).await?;

    // Output compact JSON to stdout
    println!("{}", serde_json::to_string(&report)?);

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

    eprintln!("Usage:");
    eprintln!("  ref refresh-data --url <URL>  Extract from single URL");
    eprintln!("  ref refresh-data <file.md>    Extract from all URLs in file");
    std::process::exit(2);
}

/// Extract data from multiple URLs, one at a time with a one-second pause
///
/// # Errors
/// Returns an error if the browser pool cannot be created.
pub async fn refresh_data(urls: &[String], config: &RefreshConfig) -> Result<RefreshReport> {
    let pool = BrowserPool::new(1).await?;
    let mut results = Vec::with_capacity(urls.len());

    for (i, url) in urls.iter().enumerate() {
        eprintln!("  -> {}", truncate(url, 60));
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let result = match pool.new_page().await {
            Ok(page) => extract_from_page(&page, url, config.timeout_ms).await,
            Err(e) => failure(url, None, format!("Browser error: {e}")),
        };
        results.push(result);
    }

    pool.close().await?;

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

/// Extract data from a single page; navigation errors and 4xx/5xx are failures
async fn extract_from_page(page: &BrowserPage, url: &str, timeout_ms: u64) -> ExtractedData {
    if let Err(e) = crate::guard::check_web_url(url) {
        return failure(url, None, e);
    }
    let nav = match page.goto(url, timeout_ms).await {
        Ok(nav) => nav,
        Err(e) => return failure(url, None, e.to_string()),
    };
    if let Some(error) = nav.error {
        return failure(url, nav.status, error);
    }
    if let Some(code) = nav.status.filter(|c| *c >= 400) {
        return failure(url, Some(code), format!("HTTP {code}"));
    }
    match page.content().await {
        Ok(html) => extract_numbers(url, nav.status, &html),
        Err(e) => failure(url, nav.status, e.to_string()),
    }
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
