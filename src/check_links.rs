//! check-links command: Check URL health
//!
//! LLM-optimized output - JSON compact only.

use crate::browser::BrowserPool;
use crate::extract::extract_urls;
use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use std::io::{self, BufRead};
use tokio::fs;

#[derive(Args)]
pub struct CheckLinksArgs {
    /// Markdown file to check URLs from
    #[arg(value_name = "FILE")]
    file: Option<String>,

    /// Check a single URL
    #[arg(long)]
    url: Option<String>,

    /// Read URLs from stdin (one per line)
    #[arg(long)]
    stdin: bool,

    /// Number of parallel browser tabs (1-20)
    #[arg(short, long, default_value = "5", value_parser = clap::value_parser!(u8).range(1..=20))]
    concurrency: u8,

    /// Timeout per URL in milliseconds
    #[arg(long, default_value = "15000")]
    timeout: u64,

    /// Number of retries on failure
    #[arg(long, default_value = "1")]
    retries: u8,
}

/// Configuration for check-links
pub struct CheckLinksConfig {
    pub concurrency: usize,
    pub timeout_ms: u64,
    pub retries: u8,
}

/// Result for a single link check (compact)
#[derive(Debug, Serialize)]
pub struct LinkResult {
    pub url: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_to: Option<String>,
}

/// Full report (compact)
#[derive(Debug, Serialize)]
pub struct LinkReport {
    pub ok: usize,
    pub failed: usize,
    pub results: Vec<LinkResult>,
}

/// Run the check-links command.
///
/// # Errors
///
/// Returns error if URLs can't be read or the browser pool fails.
pub async fn run_check_links(args: CheckLinksArgs) -> Result<()> {
    let urls = get_urls(&args).await?;

    if urls.is_empty() {
        eprintln!("No URLs found.");
        std::process::exit(1);
    }

    eprintln!(
        "Checking {} URLs ({} parallel)...",
        urls.len(),
        args.concurrency
    );

    let config = CheckLinksConfig {
        concurrency: args.concurrency as usize,
        timeout_ms: args.timeout,
        retries: args.retries,
    };

    let report = check_links(&urls, &config).await?;

    // Output compact JSON to stdout
    println!("{}", serde_json::to_string(&report)?);

    eprintln!("Done: {}/{} OK", report.ok, report.ok + report.failed);

    Ok(())
}

/// Get URLs from file, --url, or stdin
async fn get_urls(args: &CheckLinksArgs) -> Result<Vec<String>> {
    if let Some(url) = &args.url {
        return Ok(vec![url.clone()]);
    }

    if args.stdin {
        let stdin = io::stdin();
        let urls: Vec<String> = stdin
            .lock()
            .lines()
            .map_while(Result::ok)
            .filter(|line| line.starts_with("http"))
            .collect();
        return Ok(urls);
    }

    if let Some(file) = &args.file {
        let content = fs::read_to_string(file)
            .await
            .with_context(|| format!("Failed to read file: {file}"))?;
        return Ok(extract_urls(&content));
    }

    eprintln!("Usage:");
    eprintln!("  ref check-links <file.md>    Check URLs in markdown file");
    eprintln!("  ref check-links --url <URL>  Check single URL");
    eprintln!("  ref check-links --stdin      Read URLs from stdin");
    std::process::exit(1);
}

/// Check multiple links and generate report.
///
/// # Errors
///
/// Returns error if Chrome fails to launch or URLs can't be fetched.
pub async fn check_links(urls: &[String], config: &CheckLinksConfig) -> Result<LinkReport> {
    let pool = BrowserPool::new(config.concurrency).await?;
    let mut results = Vec::with_capacity(urls.len());
    let mut ok_count = 0;
    let mut failed_count = 0;

    for url in urls {
        eprintln!("  -> {}", truncate(url, 60));

        let page = pool.new_page().await?;
        let mut result = page.goto(url, config.timeout_ms).await?;

        // Retry on failure
        if result.status == 0 && config.retries > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            result = page.goto(url, config.timeout_ms).await?;
        }

        // Determine if redirect (check final URL)
        let redirect_to = if result.status >= 200 && result.status < 400 {
            if let Some(final_url) = page.current_url().await {
                // Check if different domain
                let orig_host = url::Url::parse(url)
                    .ok()
                    .and_then(|u| u.host_str().map(String::from));
                let final_host = url::Url::parse(&final_url)
                    .ok()
                    .and_then(|u| u.host_str().map(String::from));

                if let (Some(orig), Some(fin)) = (orig_host, final_host) {
                    let orig_norm = orig.trim_start_matches("www.");
                    let fin_norm = fin.trim_start_matches("www.");
                    if orig_norm == fin_norm {
                        None
                    } else {
                        Some(final_url)
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let is_ok = result.status >= 200 && result.status < 400 && redirect_to.is_none();
        if is_ok {
            ok_count += 1;
        } else {
            failed_count += 1;
        }

        results.push(LinkResult {
            url: url.clone(),
            status: result.status,
            error: result.error,
            redirect_to,
        });
    }

    pool.close().await?;

    Ok(LinkReport {
        ok: ok_count,
        failed: failed_count,
        results,
    })
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
    fn test_truncate() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("this is a very long string", 10), "this is...");
    }
}
