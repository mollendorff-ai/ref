//! verify-refs command: Verify references.yaml entries
//!
//! Fetches each URL and updates status:
//! - ok: 200 response, content accessible
//! - dead: 404, 5xx, DNS failure, connection error
//! - redirect: ended up on different domain (link rot indicator)
//! - paywall: 200 but content blocked by paywall
//! - login: 200 but login required

use crate::access::{self, Access};
use crate::browser::BrowserPool;
use crate::schema::{ReferencesFile, Status};
use crate::text::truncate;
use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use futures::StreamExt;
use serde::Serialize;
use std::path::PathBuf;
use tokio::sync::Mutex;
use url::Url;

#[derive(Args)]
pub struct VerifyRefsArgs {
    /// Path to references.yaml file
    pub file: PathBuf,

    /// Number of parallel browser tabs (1-20)
    #[arg(long, short, default_value = "4", value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=20))]
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

    #[command(flatten)]
    pub profile: crate::profile::ProfileArgs,
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
/// Returns `Ok(false)` when any reference is dead.
pub async fn run_verify_refs(args: VerifyRefsArgs, out: &mut impl std::io::Write) -> Result<bool> {
    let output = verify_refs_core(args, None).await?;
    writeln!(out, "{}", serde_json::to_string(&output)?)?;
    Ok(output.summary.dead == 0)
}

/// Core verification logic: read references, verify each URL, update file.
///
/// # Errors
///
/// Returns error if the references file can't be read, parsed, or written back.
///
/// Runs in `shared` when given (the MCP server's browser), else in a browser of
/// its own, started only when there is something to verify.
pub(crate) async fn verify_refs_core(
    args: VerifyRefsArgs,
    shared: Option<&BrowserPool>,
) -> Result<VerifyOutput> {
    let content = tokio::fs::read_to_string(&args.file)
        .await
        .with_context(|| format!("Failed to read {}", args.file.display()))?;

    let refs_file: ReferencesFile =
        serde_yaml::from_str(&content).context("Failed to parse references.yaml")?;

    let total = refs_file.references.len();
    eprintln!("Loaded {} references from {}", total, args.file.display());

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
        return Ok(VerifyOutput {
            summary: compute_summary(total, skipped, &[]),
            file: args.file.display().to_string(),
            timestamp: Utc::now().to_rfc3339(),
        });
    }

    eprintln!(
        "Verifying {} references ({} parallel)...",
        to_verify, args.parallel
    );

    let refs_file = Mutex::new(refs_file);
    let statuses = if let Some(pool) = shared {
        verify_all(pool, &refs_file, &indices_to_verify, &args).await
    } else {
        let pool = BrowserPool::new(args.parallel, args.profile.profile.as_deref()).await?;
        let statuses = verify_all(&pool, &refs_file, &indices_to_verify, &args).await;
        pool.close().await?;
        statuses
    };

    let summary = compute_summary(total, skipped, &statuses);

    {
        let mut file = refs_file.lock().await;
        file.meta.last_verified = Some(Utc::now().to_rfc3339());
        file.meta.total_links = file.references.len();
    }

    if args.dry_run {
        eprintln!("Dry run - file not modified");
    } else {
        let file = refs_file.lock().await;
        let yaml = serde_yaml::to_string(&*file)?;
        crate::guard::write_atomic(&args.file, yaml.as_bytes())
            .with_context(|| format!("Failed to write {}", args.file.display()))?;
        eprintln!("Updated {}", args.file.display());
    }

    Ok(VerifyOutput {
        summary,
        file: args.file.display().to_string(),
        timestamp: Utc::now().to_rfc3339(),
    })
}

/// Verify the chosen references, at most `args.parallel` at once, and record
/// each result in the file
async fn verify_all(
    pool: &BrowserPool,
    refs_file: &Mutex<ReferencesFile>,
    indices: &[usize],
    args: &VerifyRefsArgs,
) -> Vec<Status> {
    // Owned items: a stream over borrowed ones is not `Send` (needed by the MCP server)
    let timeout = args.timeout;
    futures::stream::iter(indices.to_vec())
        .map(|idx| async move {
            let url = refs_file.lock().await.references[idx].url.clone();
            eprintln!("  -> {}", truncate(&url, 60));
            let result = verify_url(pool, &url, timeout).await;

            let mut file = refs_file.lock().await;
            file.references[idx].status = result.status;
            file.references[idx].verified = Some(Utc::now().to_rfc3339());
            file.references[idx].notes = result.notes;
            result.status
        })
        .buffered(args.parallel.max(1))
        .collect()
        .await
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
    if let Err(e) = crate::guard::check_web_url(url) {
        return VerifyResult {
            status: Status::Dead,
            notes: Some(e),
        };
    }
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

    // Any 4xx or 5xx response means the reference is dead
    if let Some(code) = nav.status.filter(|c| *c >= 400) {
        return VerifyResult {
            status: Status::Dead,
            notes: Some(format!("HTTP {code}")),
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

    match access::detect(&html) {
        Access::Paywall => {
            return VerifyResult {
                status: Status::Paywall,
                notes: Some("Paywall detected".to_string()),
            }
        }
        Access::Login => {
            return VerifyResult {
                status: Status::Login,
                notes: Some("Login required".to_string()),
            }
        }
        Access::Open => {}
    }

    VerifyResult {
        status: Status::Ok,
        notes: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verify_starts_its_own_browser_when_none_is_shared() {
        // Needs Chrome (ADR-004)
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("references.yaml");
        std::fs::write(&file, "meta:\n  created: '2026-01-01'\n  last_verified: null\n  tool: ref\n  total_links: 1\nreferences:\n- id: closed-port\n  url: http://127.0.0.1:9/\n  title: Nothing listens here\n  categories: [test]\n  cited_in: []\n  status: pending\n").unwrap();
        let args = VerifyRefsArgs {
            file: file.clone(),
            parallel: 2,
            category: None,
            timeout: 5000,
            dry_run: false,
            profile: crate::profile::ProfileArgs {
                profile: Some("fresh".into()),
            },
        };
        let mut out = Vec::new();
        let all_ok = run_verify_refs(args, &mut out).await.unwrap();
        assert!(!all_ok, "a dead reference makes the run fail");
        let output: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(output["summary"]["verified"], 1);
        assert_eq!(output["summary"]["dead"], 1);
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("status: dead"), "{written}");
    }

    #[test]
    fn test_compute_summary_counts_each_status() {
        let statuses = [
            Status::Ok,
            Status::Dead,
            Status::Dead,
            Status::Paywall,
            Status::Login,
            Status::Redirect,
        ];
        let summary = compute_summary(8, 2, &statuses);
        assert_eq!(summary.total, 8);
        assert_eq!(summary.verified, 6);
        assert_eq!(summary.ok, 1);
        assert_eq!(summary.dead, 2);
        assert_eq!(summary.paywall, 1);
        assert_eq!(summary.login, 1);
        assert_eq!(summary.redirect, 1);
        assert_eq!(summary.skipped, 2);
    }
}
