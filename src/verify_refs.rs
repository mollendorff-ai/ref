//! verify-refs command: Verify references.yaml entries
//!
//! Fetches each URL and updates status:
//! - ok: 200 response, content accessible
//! - dead: 404, 5xx, DNS failure, connection error
//! - redirect: ended up on different domain (link rot indicator)
//! - paywall: 200 but content blocked by paywall
//! - login: 200 but login required

use crate::browser::BrowserPool;
use crate::schema::{ReferencesFile, Status};
use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use futures::future::join_all;
use scraper::{Html, Selector};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use url::Url;

#[derive(Args)]
pub struct VerifyRefsArgs {
    /// Path to references.yaml file
    pub file: PathBuf,

    /// Number of parallel browser tabs
    #[arg(long, short, default_value = "4")]
    pub parallel: usize,

    /// Filter by category (can be used multiple times)
    #[arg(long, short)]
    pub category: Option<Vec<String>>,

    /// Timeout per URL in milliseconds
    #[arg(long, default_value = "30000")]
    pub timeout: u64,

    /// Dry run - don't write changes back to file
    #[arg(long)]
    pub dry_run: bool,
}

/// Summary of verification results
#[derive(Debug, Serialize)]
pub struct VerifySummary {
    pub total: usize,
    pub verified: usize,
    pub ok: usize,
    pub dead: usize,
    pub redirect: usize,
    pub paywall: usize,
    pub login: usize,
    pub skipped: usize,
}

/// Output for JSON
#[derive(Debug, Serialize)]
pub struct VerifyOutput {
    pub summary: VerifySummary,
    pub file: String,
    pub timestamp: String,
}

/// Run the verify-refs command.
///
/// # Errors
///
/// Returns error if the references file can't be read, parsed, or written back.
pub async fn run_verify_refs(args: VerifyRefsArgs) -> Result<()> {
    // Read and parse references.yaml
    let content = tokio::fs::read_to_string(&args.file)
        .await
        .with_context(|| format!("Failed to read {}", args.file.display()))?;

    let refs_file: ReferencesFile =
        serde_yaml::from_str(&content).context("Failed to parse references.yaml")?;

    let total = refs_file.references.len();
    eprintln!("Loaded {} references from {}", total, args.file.display());

    // Filter by category if specified
    let indices_to_verify: Vec<usize> = refs_file
        .references
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            if let Some(cats) = &args.category {
                r.categories.iter().any(|c| cats.contains(c))
            } else {
                true
            }
        })
        .map(|(i, _)| i)
        .collect();

    let to_verify = indices_to_verify.len();
    let skipped = total - to_verify;

    if to_verify == 0 {
        eprintln!("No references to verify (all filtered out)");
        return Ok(());
    }

    eprintln!(
        "Verifying {} references ({} parallel)...",
        to_verify, args.parallel
    );

    // Create browser pool
    let pool = Arc::new(BrowserPool::new(args.parallel).await?);
    let timeout = args.timeout;

    // Shared mutable references for updating
    let refs_file = Arc::new(Mutex::new(refs_file));

    // Verify each reference
    let tasks: Vec<_> = indices_to_verify
        .into_iter()
        .map(|idx| {
            let pool = Arc::clone(&pool);
            let refs_file = Arc::clone(&refs_file);
            tokio::spawn(async move {
                let url = {
                    let file = refs_file.lock().await;
                    file.references[idx].url.clone()
                };

                eprintln!("  -> {}", truncate(&url, 60));
                let result = verify_url(&pool, &url, timeout).await;

                // Update the reference
                {
                    let mut file = refs_file.lock().await;
                    file.references[idx].status = result.status;
                    file.references[idx].verified = Some(Utc::now().to_rfc3339());
                    file.references[idx].notes = result.notes;
                }

                result.status
            })
        })
        .collect();

    // Await all tasks
    let statuses: Vec<Status> = join_all(tasks)
        .await
        .into_iter()
        .filter_map(std::result::Result::ok)
        .collect();

    // Close browser
    if let Ok(pool) = Arc::try_unwrap(pool) {
        pool.close().await?;
    }

    let summary = compute_summary(total, skipped, &statuses);

    // Update meta
    {
        let mut file = refs_file.lock().await;
        file.meta.last_verified = Some(Utc::now().to_rfc3339());
        file.meta.total_links = file.references.len();
    }

    // Write back to file (unless dry run)
    if args.dry_run {
        eprintln!("Dry run - file not modified");
    } else {
        let file = refs_file.lock().await;
        let yaml = serde_yaml::to_string(&*file)?;
        tokio::fs::write(&args.file, yaml)
            .await
            .with_context(|| format!("Failed to write {}", args.file.display()))?;
        eprintln!("Updated {}", args.file.display());
    }

    // Output JSON summary
    let output = VerifyOutput {
        summary,
        file: args.file.display().to_string(),
        timestamp: Utc::now().to_rfc3339(),
    };
    println!("{}", serde_json::to_string(&output)?);

    Ok(())
}

/// Compute a summary from the list of verification statuses.
fn compute_summary(total: usize, skipped: usize, statuses: &[Status]) -> VerifySummary {
    let mut summary = VerifySummary {
        total,
        verified: statuses.len(),
        ok: 0,
        dead: 0,
        redirect: 0,
        paywall: 0,
        login: 0,
        skipped,
    };

    for status in statuses {
        match status {
            Status::Ok => summary.ok += 1,
            Status::Dead => summary.dead += 1,
            Status::Redirect => summary.redirect += 1,
            Status::Paywall => summary.paywall += 1,
            Status::Login => summary.login += 1,
            Status::Pending => {}
        }
    }

    summary
}

/// Result of verifying a single URL
struct VerifyResult {
    status: Status,
    notes: Option<String>,
}

async fn verify_url(pool: &BrowserPool, url: &str, timeout: u64) -> VerifyResult {
    let page = match pool.new_page().await {
        Ok(p) => p,
        Err(e) => {
            return VerifyResult {
                status: Status::Dead,
                notes: Some(format!("Browser error: {e}")),
            }
        }
    };

    // Parse original URL to get host
    let original_host = match Url::parse(url) {
        Ok(u) => u.host_str().map(std::string::ToString::to_string),
        Err(_) => None,
    };

    let nav = match page.goto(url, timeout).await {
        Ok(n) => n,
        Err(e) => {
            return VerifyResult {
                status: Status::Dead,
                notes: Some(format!("Navigation error: {e}")),
            }
        }
    };

    // Check for navigation errors (DNS, connection, etc.)
    if nav.error.is_some() {
        return VerifyResult {
            status: Status::Dead,
            notes: nav.error,
        };
    }

    // Check HTTP status heuristics
    if nav.status == 404 || nav.status >= 500 {
        return VerifyResult {
            status: Status::Dead,
            notes: Some(format!("HTTP {}", nav.status)),
        };
    }

    // Get final URL to check for cross-domain redirect
    let final_url = page.current_url().await;
    if let (Some(orig), Some(final_u)) = (&original_host, &final_url) {
        if let Ok(parsed) = Url::parse(final_u) {
            if let Some(final_host) = parsed.host_str() {
                // Normalize hosts (remove www prefix for comparison)
                let orig_norm = orig.trim_start_matches("www.");
                let final_norm = final_host.trim_start_matches("www.");
                if orig_norm != final_norm {
                    return VerifyResult {
                        status: Status::Redirect,
                        notes: Some(final_u.clone()),
                    };
                }
            }
        }
    }

    // Get page content to detect paywall/login
    let Ok(html) = page.content().await else {
        return VerifyResult {
            status: Status::Ok,
            notes: None,
        };
    };

    // Check for paywall indicators
    if is_paywall(&html) {
        return VerifyResult {
            status: Status::Paywall,
            notes: Some("Paywall detected".to_string()),
        };
    }

    // Check for login wall indicators
    if is_login_wall(&html) {
        return VerifyResult {
            status: Status::Login,
            notes: Some("Login required".to_string()),
        };
    }

    VerifyResult {
        status: Status::Ok,
        notes: None,
    }
}

/// Detect paywall indicators in HTML
fn is_paywall(html: &str) -> bool {
    let doc = Html::parse_document(html);
    let lower = html.to_lowercase();

    // Common paywall indicators
    let paywall_patterns = [
        "subscribe to continue",
        "subscription required",
        "premium content",
        "paywall",
        "member-only",
        "members only",
        "unlock this article",
        "purchase to read",
        "buy now to read",
        "paid subscribers",
    ];

    for pattern in paywall_patterns {
        if lower.contains(pattern) {
            return true;
        }
    }

    // Check for paywall-specific CSS classes/IDs
    let paywall_selectors = [
        "[class*='paywall']",
        "[id*='paywall']",
        "[class*='subscription-wall']",
        "[class*='piano-offer']",
        "[class*='premium-wall']",
    ];

    for sel_str in paywall_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if doc.select(&sel).next().is_some() {
                return true;
            }
        }
    }

    false
}

/// Detect login wall indicators in HTML
fn is_login_wall(html: &str) -> bool {
    let doc = Html::parse_document(html);
    let lower = html.to_lowercase();

    // Common login wall indicators
    let login_patterns = [
        "sign in to continue",
        "log in to continue",
        "login to continue",
        "please sign in",
        "please log in",
        "create an account",
        "sign up to view",
        "register to view",
        "authentication required",
    ];

    for pattern in login_patterns {
        if lower.contains(pattern) {
            return true;
        }
    }

    // Check for login-specific elements that block content
    let login_selectors = [
        "[class*='login-wall']",
        "[class*='auth-wall']",
        "[class*='signup-wall']",
        "[id*='login-modal']",
        "[class*='gate-content']",
    ];

    for sel_str in login_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if doc.select(&sel).next().is_some() {
                return true;
            }
        }
    }

    false
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
    fn test_is_paywall() {
        assert!(is_paywall("<div>Subscribe to continue reading</div>"));
        assert!(is_paywall("<div class='paywall-overlay'>content</div>"));
        assert!(!is_paywall("<div>Normal content here</div>"));
    }

    #[test]
    fn test_is_login_wall() {
        assert!(is_login_wall("<div>Please sign in to continue</div>"));
        assert!(is_login_wall("<div class='login-wall'>content</div>"));
        assert!(!is_login_wall("<div>Normal content here</div>"));
    }
}
