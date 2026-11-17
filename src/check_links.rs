//! check-links command: Check URL health
//!
//! LLM-optimized output - JSON compact only.

use crate::browser::{page_failure, BrowserPool};
use crate::extract::extract_urls;
use crate::text::truncate;
use anyhow::{Context, Result};
use clap::Args;
use futures::{StreamExt, TryStreamExt};
use serde::Serialize;
use std::io::{self, BufRead};
use std::sync::Arc;
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

    /// Send this exact user agent instead of Chrome's own with ref's name added
    /// (some sites want a name and an email: --user-agent "Jane Doe jane@example.com")
    #[arg(long, value_name = "TEXT")]
    user_agent: Option<String>,

    #[command(flatten)]
    profile: crate::profile::ProfileArgs,
}

/// Configuration for check-links
pub struct CheckLinksConfig {
    pub concurrency: usize,
    pub timeout_ms: u64,
    pub retries: u8,
    pub user_agent: Option<String>,
    /// `--profile` value, or `None` for the default
    pub profile: Option<crate::profile::ProfileChoice>,
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

impl LinkResult {
    /// A check that failed inside ref itself (a panic), not at the site
    fn internal_error(url: String, message: &str) -> Self {
        Self {
            url,
            status: 0,
            error: Some(format!("Internal error in ref: {message}")),
            redirect_to: None,
        }
    }
}

/// Full report (compact)
#[derive(Debug, Default, Serialize)]
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
/// Returns `Ok(false)` when any link failed.
pub async fn run_check_links(args: CheckLinksArgs, out: &mut impl std::io::Write) -> Result<bool> {
    let urls = get_urls(&args).await?;

    if urls.is_empty() {
        eprintln!("No URLs found.");
        return Ok(false);
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
        user_agent: args.user_agent.clone(),
        profile: args.profile.choice()?,
    };

    let report = check_links(&urls, &config).await?;

    // Output compact JSON to stdout
    writeln!(out, "{}", serde_json::to_string(&report)?)?;

    eprintln!("Done: {}/{} OK", report.ok, report.ok + report.failed);

    Ok(report.failed == 0)
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

    anyhow::bail!(
        "Usage:\n  ref check-links <file.md>    Check URLs in markdown file\n  \
         ref check-links --url <URL>  Check single URL\n  \
         ref check-links --stdin      Read URLs from stdin"
    )
}

/// Check multiple links in a browser of their own and generate a report.
///
/// # Errors
///
/// Returns error if Chrome fails to launch, fails while checking
/// ([`ChromeFailed`](crate::browser::ChromeFailed)), or the user agent is invalid.
pub async fn check_links(urls: &[String], config: &CheckLinksConfig) -> Result<LinkReport> {
    let mut pool = BrowserPool::new(config.concurrency, config.profile.as_ref()).await?;
    if let Some(ua) = &config.user_agent {
        pool.set_user_agent(ua)?;
    }
    let pool = Arc::new(pool);
    let report = check_links_with(
        &pool,
        urls,
        config.concurrency,
        config.timeout_ms,
        config.retries,
    )
    .await;
    pool.stop().await;
    report
}

/// Check links in a browser the caller already has, such as the MCP server's
/// shared one, at most `concurrency` at once, each on a task of its own so a
/// panic on one URL becomes that URL's error.
///
/// # Errors
/// Returns [`ChromeFailed`](crate::browser::ChromeFailed) when Chrome fails,
/// and stops checking then.
pub async fn check_links_with(
    pool: &Arc<BrowserPool>,
    urls: &[String],
    concurrency: usize,
    timeout_ms: u64,
    retries: u8,
) -> Result<LinkReport> {
    let results: Vec<LinkResult> = futures::stream::iter(urls.to_vec())
        .map(|url| {
            let pool = Arc::clone(pool);
            async move {
                let checked = crate::task::isolated({
                    let url = url.clone();
                    async move { check_one(&pool, &url, timeout_ms, retries).await }
                })
                .await;
                checked.unwrap_or_else(|e| Ok(LinkResult::internal_error(url, &e)))
            }
        })
        .buffered(concurrency.max(1))
        .try_collect()
        .await?;

    let ok = results
        .iter()
        .filter(|r| {
            r.error.is_none() && r.redirect_to.is_none() && r.status >= 200 && r.status < 400
        })
        .count();
    let failed = results.len() - ok;

    Ok(LinkReport {
        ok,
        failed,
        results,
    })
}

/// Check one link; `Err` only for a Chrome failure
async fn check_one(pool: &BrowserPool, url: &str, timeout: u64, retries: u8) -> Result<LinkResult> {
    eprintln!("  -> {}", truncate(url, 60));
    let failed = |error: String| LinkResult {
        url: url.to_string(),
        status: 0,
        error: Some(error),
        redirect_to: None,
    };

    if let Err(e) = crate::guard::check_web_url(url) {
        return Ok(failed(e));
    }

    let page = match page_failure(pool.new_page().await)? {
        Ok(p) => p,
        Err(e) => return Ok(failed(e)),
    };

    let mut result = match page_failure(page.goto(url, timeout).await)? {
        Ok(r) => r,
        Err(e) => return Ok(failed(e)),
    };

    // Retry on failure (no HTTP response at all)
    if result.status.is_none() && retries > 0 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if let Ok(r) = page_failure(page.goto(url, timeout).await)? {
            result = r;
        }
    }

    // Determine if redirect (check final URL)
    let status = result.status.unwrap_or(0);
    let redirect_to = if (200..400).contains(&status) {
        if let Some(final_url) = page.current_url().await? {
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

    Ok(LinkResult {
        url: url.to_string(),
        status,
        error: result.error,
        redirect_to,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_tab_chrome_refuses_is_that_links_error() {
        // Needs Chrome (ADR-004)
        let pool = crate::browser::tests::pool_that_cannot_set_up_tabs().await;
        let urls = ["http://127.0.0.1:9/".to_string()];
        let report = check_links_with(&pool, &urls, 1, 5000, 0).await.unwrap();
        let error = report.results[0].error.clone().unwrap_or_default();
        assert!(error.contains("cookies"), "{error}");
        pool.stop().await;
    }

    #[tokio::test]
    async fn a_chrome_failure_ends_the_check() {
        // Needs Chrome (ADR-004)
        let pool = Arc::new(
            BrowserPool::new(2, Some(&crate::profile::ProfileChoice::Fresh))
                .await
                .unwrap(),
        );
        let trap = crate::browser::tests::serve_trap(&pool, "-KILL").await;
        let urls = [trap, "http://127.0.0.1:9/".to_string()];
        let err = check_links_with(&pool, &urls, 2, 10_000, 1)
            .await
            .unwrap_err();
        assert!(err.is::<crate::browser::ChromeFailed>(), "{err:#}");
        pool.stop().await;
    }

    #[test]
    fn internal_errors_say_ref_failed_not_the_site() {
        let r = LinkResult::internal_error("https://a.example".into(), "bad page");
        assert_eq!(r.status, 0);
        assert_eq!(r.error.as_deref(), Some("Internal error in ref: bad page"));
        assert!(r.redirect_to.is_none());
    }

    #[test]
    fn test_check_links_config() {
        let config = CheckLinksConfig {
            concurrency: 5,
            timeout_ms: 15000,
            retries: 1,
            user_agent: None,
            profile: None,
        };
        assert_eq!(config.concurrency, 5);
        assert_eq!(config.timeout_ms, 15000);
        assert_eq!(config.retries, 1);
    }

    #[test]
    fn test_truncate_non_ascii_url() {
        // Used to panic: byte slicing inside a multi-byte character
        let url = format!("https://exemple.ca/{}", "é".repeat(40));
        assert!(truncate(&url, 60).ends_with("..."));
    }
}
