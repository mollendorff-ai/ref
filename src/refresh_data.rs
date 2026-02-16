//! refresh-data command: Extract live data from URLs
//!
//! LLM-optimized output - JSON compact only.

use crate::browser::BrowserPool;
use crate::extract::{extract_amounts, extract_percentages, AmountMatch};
use anyhow::{Context, Result};
use clap::Args;
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
    #[serde(rename = "type")]
    pub extractor_type: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amounts: Option<Vec<AmountMatch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentages: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub followers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
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
/// # Errors
/// Returns an error if URLs cannot be resolved, the browser fails to launch, or JSON serialization fails.
pub async fn run_refresh_data(args: RefreshDataArgs) -> Result<()> {
    let urls = get_extractable_urls(&args).await?;

    if urls.is_empty() {
        eprintln!("No extractable URLs found.");
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

    Ok(())
}

/// Determine extractor type from URL (auto-detect)
fn get_extractor_type(url: &str) -> &'static str {
    if url.contains("instagram.com") {
        "instagram"
    } else if url.contains("statista.com") {
        "statista"
    } else {
        "generic"
    }
}

/// Get URLs to extract from
async fn get_extractable_urls(args: &RefreshDataArgs) -> Result<Vec<(String, String)>> {
    if let Some(url) = &args.url {
        let ext_type = get_extractor_type(url);
        return Ok(vec![(url.clone(), ext_type.to_string())]);
    }

    if let Some(file) = &args.file {
        let content = fs::read_to_string(file)
            .await
            .with_context(|| format!("Failed to read file: {file}"))?;

        return Ok(extract_extractable_urls(&content));
    }

    eprintln!("Usage:");
    eprintln!("  ref refresh-data --url <URL>  Extract from single URL");
    eprintln!("  ref refresh-data <file.md>    Extract from all URLs in file");
    std::process::exit(1);
}

/// Extract URLs that have extractors
fn extract_extractable_urls(content: &str) -> Vec<(String, String)> {
    use regex::Regex;
    use std::collections::HashSet;

    let patterns = [
        (Regex::new(r"https?://(?:www\.)?instagram\.com/[^\s\)\]]+").unwrap(), "instagram"),
        (Regex::new(r"https?://(?:www\.)?statista\.com/[^\s\)\]]+").unwrap(), "statista"),
        (Regex::new(r"https?://(?:www\.)?(?:influencermarketinghub|emarketer|techcrunch)\.com/[^\s\)\]]+").unwrap(), "market"),
    ];

    let mut seen = HashSet::new();
    let mut urls = Vec::new();

    for (re, ext_type) in &patterns {
        for mat in re.find_iter(content) {
            let url = mat.as_str().trim_end_matches([',', '.', ')', ']']);
            if !seen.contains(url) {
                seen.insert(url.to_string());
                urls.push((url.to_string(), (*ext_type).to_string()));
            }
        }
    }

    urls
}

/// Extract data from multiple URLs
///
/// # Errors
/// Returns an error if the browser pool cannot be created or page navigation fails.
pub async fn refresh_data(
    urls: &[(String, String)],
    config: &RefreshConfig,
) -> Result<RefreshReport> {
    let pool = BrowserPool::new(1).await?; // Sequential for rate limiting
    let mut results = Vec::with_capacity(urls.len());
    let mut ok_count = 0;
    let mut failed_count = 0;

    for (url, ext_type) in urls {
        eprintln!("  -> [{}] {}", ext_type, truncate(url, 50));

        let page = pool.new_page().await?;
        let result = extract_from_page(&page, url, ext_type, config.timeout_ms).await;

        if result.success {
            ok_count += 1;
        } else {
            failed_count += 1;
        }

        results.push(result);

        // Rate limit
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    pool.close().await?;

    Ok(RefreshReport {
        ok: ok_count,
        failed: failed_count,
        results,
    })
}

/// Extract data from a single page
async fn extract_from_page(
    page: &crate::browser::BrowserPage,
    url: &str,
    ext_type: &str,
    timeout_ms: u64,
) -> ExtractedData {
    let nav_result = page.goto(url, timeout_ms).await;
    if let Err(e) = nav_result {
        return ExtractedData {
            url: url.to_string(),
            extractor_type: ext_type.to_string(),
            success: false,
            title: None,
            amounts: None,
            percentages: None,
            followers: None,
            username: None,
            error: Some(e.to_string()),
        };
    }

    let content = match page.content().await {
        Ok(c) => c,
        Err(e) => {
            return ExtractedData {
                url: url.to_string(),
                extractor_type: ext_type.to_string(),
                success: false,
                title: None,
                amounts: None,
                percentages: None,
                followers: None,
                username: None,
                error: Some(e.to_string()),
            };
        }
    };

    match ext_type {
        "instagram" => extract_instagram(url, &content),
        "statista" => extract_statista(url, &content),
        _ => extract_generic(url, &content),
    }
}

fn extract_instagram(url: &str, content: &str) -> ExtractedData {
    use regex::Regex;

    let follower_re = Regex::new(r"([0-9,.]+[KMB]?)\s*[Ff]ollowers").unwrap();
    let followers = follower_re.captures(content).map(|c| c[1].to_string());

    // Extract username from URL
    let username = url
        .trim_end_matches('/')
        .split('/')
        .next_back()
        .map(std::string::ToString::to_string);

    ExtractedData {
        url: url.to_string(),
        extractor_type: "instagram".to_string(),
        success: true,
        title: None,
        amounts: None,
        percentages: None,
        followers,
        username,
        error: None,
    }
}

fn extract_statista(url: &str, content: &str) -> ExtractedData {
    let amounts = extract_amounts(content);
    let percentages = extract_percentages(content);
    let title = extract_title(content);

    ExtractedData {
        url: url.to_string(),
        extractor_type: "statista".to_string(),
        success: true,
        title,
        amounts: if amounts.is_empty() {
            None
        } else {
            Some(amounts)
        },
        percentages: if percentages.is_empty() {
            None
        } else {
            Some(percentages)
        },
        followers: None,
        username: None,
        error: None,
    }
}

fn extract_generic(url: &str, content: &str) -> ExtractedData {
    let amounts = extract_amounts(content);
    let percentages = extract_percentages(content);
    let title = extract_title(content);

    ExtractedData {
        url: url.to_string(),
        extractor_type: "generic".to_string(),
        success: true,
        title,
        amounts: if amounts.is_empty() {
            None
        } else {
            Some(amounts)
        },
        percentages: if percentages.is_empty() {
            None
        } else {
            Some(percentages)
        },
        followers: None,
        username: None,
        error: None,
    }
}

fn extract_title(content: &str) -> Option<String> {
    use regex::Regex;

    // Try <h1> first
    let h1_re = Regex::new(r"<h1[^>]*>([^<]+)</h1>").unwrap();
    if let Some(cap) = h1_re.captures(content) {
        return Some(cap[1].trim().to_string());
    }

    // Fall back to <title>
    let title_re = Regex::new(r"<title[^>]*>([^<]+)</title>").unwrap();
    title_re.captures(content).map(|c| c[1].trim().to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_extractor_type() {
        assert_eq!(
            get_extractor_type("https://instagram.com/user"),
            "instagram"
        );
        assert_eq!(
            get_extractor_type("https://www.statista.com/stats"),
            "statista"
        );
        assert_eq!(get_extractor_type("https://example.com"), "generic");
    }

    #[test]
    fn test_extract_title() {
        let html =
            "<html><head><title>Test Page</title></head><body><h1>Main Title</h1></body></html>";
        assert_eq!(extract_title(html), Some("Main Title".to_string()));

        let html_no_h1 = "<html><head><title>Test Page</title></head></html>";
        assert_eq!(extract_title(html_no_h1), Some("Test Page".to_string()));
    }

    #[test]
    fn test_extract_instagram() {
        let content = "Profile has 577K Followers and 100 posts";
        let result = extract_instagram("https://instagram.com/testuser", content);
        assert_eq!(result.followers, Some("577K".to_string()));
        assert_eq!(result.username, Some("testuser".to_string()));
    }

    #[test]
    fn test_extract_extractable_urls() {
        let content = r"
            Check https://instagram.com/user1 and
            https://www.statista.com/statistics/123
            and https://example.com for more.
        ";

        let urls = extract_extractable_urls(content);
        assert_eq!(urls.len(), 2); // Only instagram and statista
        assert!(urls.iter().any(|(u, _)| u.contains("instagram")));
        assert!(urls.iter().any(|(u, _)| u.contains("statista")));
    }
}
