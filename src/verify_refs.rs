//! verify-refs command: Verify references.yaml entries
//!
//! Fetches each URL and updates status:
//! - ok: 200 response, content accessible
//! - dead: 404, 5xx, DNS failure, connection error
//! - redirect: ended up on different domain (link rot indicator)
//! - paywall: 200 but content blocked by paywall
//! - login: 200 but login required

use crate::access::{self, Access};
use crate::browser::{page_failure, BrowserPool};
use crate::schema::{ReferencesFile, Status};
use crate::text::truncate;
use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use futures::{StreamExt, TryStreamExt};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
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
/// Returns error if the references file can't be read, parsed, or written
/// back, or Chrome fails (then the file is left as it was).
/// Returns `Ok(false)` when any reference is dead.
pub async fn run_verify_refs(args: VerifyRefsArgs, out: &mut impl std::io::Write) -> Result<bool> {
    let output = verify_refs_core(&args).await?;
    writeln!(out, "{}", serde_json::to_string(&output)?)?;
    Ok(output.summary.dead == 0)
}

/// Verify in a browser of its own, started only when there is something to verify.
///
/// # Errors
///
/// Returns error if the references file can't be read, parsed, or written
/// back, or Chrome fails (then the file is left as it was).
pub(crate) async fn verify_refs_core(args: &VerifyRefsArgs) -> Result<VerifyOutput> {
    let plan = Plan::load(&args.file, args.category.as_deref()).await?;
    if plan.is_empty() {
        return Ok(plan.nothing_to_verify());
    }
    let profile = args.profile.choice()?;
    let pool = Arc::new(BrowserPool::new(args.parallel, profile.as_ref()).await?);
    let output = plan
        .run(&pool, args.parallel, args.timeout, args.dry_run)
        .await;
    // Stopping never fails, so the run's own error is the one reported
    pool.stop().await?;
    output
}

/// The references file and the entries chosen to verify
pub(crate) struct Plan {
    file: PathBuf,
    refs: ReferencesFile,
    chosen: Vec<usize>,
}

impl Plan {
    /// Read and parse the file and choose the entries (all, or those in `categories`)
    ///
    /// # Errors
    /// Returns error if the file can't be read or parsed.
    pub(crate) async fn load(
        file: &std::path::Path,
        categories: Option<&[String]>,
    ) -> Result<Self> {
        let content = tokio::fs::read_to_string(file)
            .await
            .with_context(|| format!("Failed to read {}", file.display()))?;
        let refs: ReferencesFile =
            serde_yaml::from_str(&content).context("Failed to parse references.yaml")?;
        eprintln!(
            "Loaded {} references from {}",
            refs.references.len(),
            file.display()
        );

        let chosen = refs
            .references
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                categories.is_none_or(|cats| r.categories.iter().any(|c| cats.contains(c)))
            })
            .map(|(i, _)| i)
            .collect();
        Ok(Self {
            file: file.to_path_buf(),
            refs,
            chosen,
        })
    }

    /// True when no entry is chosen, so no browser is needed
    pub(crate) fn is_empty(&self) -> bool {
        self.chosen.is_empty()
    }

    fn output(&self, statuses: &[Status]) -> VerifyOutput {
        let total = self.refs.references.len();
        VerifyOutput {
            summary: compute_summary(total, total - self.chosen.len(), statuses),
            file: self.file.display().to_string(),
            timestamp: Utc::now().to_rfc3339(),
        }
    }

    /// The summary when nothing is chosen; the file is not touched
    pub(crate) fn nothing_to_verify(&self) -> VerifyOutput {
        eprintln!("No references to verify (all filtered out)");
        self.output(&[])
    }

    /// Verify the chosen entries in `pool`, at most `parallel` at once, each on
    /// a task of its own, then write the file (unless `dry_run`).
    ///
    /// # Errors
    /// When Chrome fails or a check panics, returns the error with its URL and
    /// leaves the file as it was; also when the file can't be written.
    pub(crate) async fn run(
        mut self,
        pool: &Arc<BrowserPool>,
        parallel: usize,
        timeout: u64,
        dry_run: bool,
    ) -> Result<VerifyOutput> {
        eprintln!(
            "Verifying {} references ({parallel} parallel)...",
            self.chosen.len()
        );
        // Stops at the first failure: nothing will be written, so checking the
        // rest would only take time (up to the request timeout each on a hung Chrome)
        let file = &self.file;
        let checks: Vec<(usize, VerifyResult)> = futures::stream::iter(self.chosen.clone())
            .map(|idx| {
                let (pool, url) = (Arc::clone(pool), self.refs.references[idx].url.clone());
                async move {
                    eprintln!("  -> {}", truncate(&url, 60));
                    let checked = crate::task::isolated({
                        let url = url.clone();
                        async move { verify_url(&pool, &url, timeout).await }
                    })
                    .await;
                    let checked = checked
                        .unwrap_or_else(|e| Err(anyhow::anyhow!("Internal error in ref: {e}")));
                    checked.map(|result| (idx, result)).map_err(|e| {
                        anyhow::anyhow!("{url}: {e}. {} was not changed.", file.display())
                    })
                }
            })
            .buffered(parallel.max(1))
            .try_collect()
            .await?;

        let mut statuses = Vec::with_capacity(checks.len());
        for (idx, result) in checks {
            let entry = &mut self.refs.references[idx];
            entry.status = result.status;
            entry.verified = Some(Utc::now().to_rfc3339());
            entry.notes = result.notes;
            statuses.push(result.status);
        }
        self.refs.meta.last_verified = Some(Utc::now().to_rfc3339());
        self.refs.meta.total_links = self.refs.references.len();

        if dry_run {
            eprintln!("Dry run - file not modified");
        } else {
            let yaml = serde_yaml::to_string(&self.refs)?;
            crate::guard::write_atomic(&self.file, yaml.as_bytes())
                .with_context(|| format!("Failed to write {}", self.file.display()))?;
            eprintln!("Updated {}", self.file.display());
        }
        Ok(self.output(&statuses))
    }
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

/// Verify one URL. `Err` means Chrome itself failed
/// ([`ChromeFailed`](crate::browser::ChromeFailed)), which says nothing about
/// the URL, so nothing may be recorded for it (ADR-005 rule 3).
async fn verify_url(pool: &BrowserPool, url: &str, timeout: u64) -> Result<VerifyResult> {
    let dead = |notes: String| VerifyResult {
        status: Status::Dead,
        notes: Some(notes),
    };
    if let Err(e) = crate::guard::check_web_url(url) {
        return Ok(dead(e));
    }
    let page = match page_failure(pool.new_page().await)? {
        Ok(page) => page,
        Err(e) => return Ok(dead(format!("Browser error: {e}"))),
    };
    let nav = match page_failure(page.goto(url, timeout).await)? {
        Ok(nav) => nav,
        Err(e) => return Ok(dead(format!("Navigation error: {e}"))),
    };
    judge(&page, url, nav).await
}

/// The status of a page Chrome loaded
async fn judge(
    page: &crate::browser::BrowserPage,
    url: &str,
    nav: crate::browser::PageResult,
) -> Result<VerifyResult> {
    let original_host = Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(std::string::ToString::to_string));

    // Check for navigation errors (DNS, connection, etc.)
    if nav.error.is_some() {
        return Ok(VerifyResult {
            status: Status::Dead,
            notes: nav.error,
        });
    }

    // Any 4xx or 5xx response means the reference is dead
    if let Some(code) = nav.status.filter(|c| *c >= 400) {
        return Ok(VerifyResult {
            status: Status::Dead,
            notes: Some(format!("HTTP {code}")),
        });
    }

    // Get final URL to check for cross-domain redirect
    let final_url = page.current_url().await?;
    if let (Some(orig), Some(final_u)) = (&original_host, &final_url) {
        if let Ok(parsed) = Url::parse(final_u) {
            if let Some(final_host) = parsed.host_str() {
                // Normalize hosts (remove www prefix for comparison)
                let orig_norm = orig.trim_start_matches("www.");
                let final_norm = final_host.trim_start_matches("www.");
                if orig_norm != final_norm {
                    return Ok(VerifyResult {
                        status: Status::Redirect,
                        notes: Some(final_u.clone()),
                    });
                }
            }
        }
    }

    // Get page content to detect paywall/login
    let Ok(html) = page_failure(page.content().await)? else {
        return Ok(VerifyResult {
            status: Status::Ok,
            notes: None,
        });
    };

    match access::detect(&html) {
        Access::Paywall => {
            return Ok(VerifyResult {
                status: Status::Paywall,
                notes: Some("Paywall detected".to_string()),
            })
        }
        Access::Login => {
            return Ok(VerifyResult {
                status: Status::Login,
                notes: Some("Login required".to_string()),
            })
        }
        Access::Open => {}
    }

    Ok(VerifyResult {
        status: Status::Ok,
        notes: None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[tokio::test]
    async fn a_tab_chrome_refuses_is_recorded_for_that_url() {
        // Needs Chrome (ADR-004). Chrome itself is fine, so this is the page's failure.
        let (_dir, file) = one_reference();
        let pool = crate::browser::tests::pool_that_cannot_set_up_tabs().await;
        let plan = Plan::load(&file, None).await.unwrap();
        let output = plan.run(&pool, 1, 5000, true).await.unwrap();
        assert_eq!(output.summary.dead, 1);
        pool.stop().await.unwrap();
    }

    #[tokio::test]
    async fn verify_starts_its_own_browser_when_none_is_shared() {
        // Needs Chrome (ADR-004)
        let (_dir, file) = one_reference();
        let args = args_for(&file, None);
        let mut out = Vec::new();
        let all_ok = run_verify_refs(args, &mut out).await.unwrap();
        assert!(!all_ok, "a dead reference makes the run fail");
        let output: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(output["summary"]["verified"], 1);
        assert_eq!(output["summary"]["dead"], 1);
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("status: dead"), "{written}");
    }

    /// Write a references file with one entry, at a closed port
    pub(crate) fn write_one_reference(file: &std::path::Path) {
        write_reference(file, "http://127.0.0.1:9/");
    }

    /// Write a references file with one entry, at `url`
    fn write_reference(file: &std::path::Path, url: &str) {
        let yaml = format!(
            "meta:\n  created: '2026-01-01'\n  last_verified: null\n  tool: ref\n  total_links: 1\n\
             references:\n- id: one\n  url: {url}\n  title: One\n  categories: [test]\n  \
             cited_in: []\n  status: pending\n"
        );
        std::fs::write(file, yaml).unwrap();
    }

    fn args_for(file: &std::path::Path, category: Option<Vec<String>>) -> VerifyRefsArgs {
        VerifyRefsArgs {
            file: file.to_path_buf(),
            parallel: 2,
            category,
            timeout: 5000,
            dry_run: false,
            profile: crate::profile::ProfileArgs {
                profile: Some(crate::profile::ProfileChoice::Fresh),
            },
        }
    }

    #[tokio::test]
    async fn the_command_needs_no_browser_when_nothing_is_chosen() {
        let (_dir, file) = one_reference();
        let output = verify_refs_core(&args_for(&file, Some(vec!["other".into()])))
            .await
            .unwrap();
        assert_eq!(output.summary.skipped, 1);
    }

    #[tokio::test]
    async fn a_reference_that_is_not_a_web_page_is_dead() {
        // Needs Chrome (ADR-004)
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("references.yaml");
        write_reference(&file, "file:///etc/hostname");
        let output = verify_refs_core(&args_for(&file, None)).await.unwrap();
        assert_eq!(output.summary.dead, 1);
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("Only http and https"), "{written}");
    }

    fn one_reference() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("references.yaml");
        write_one_reference(&file);
        (dir, file)
    }

    #[tokio::test]
    async fn a_chrome_failure_changes_nothing_in_the_file() {
        // Needs Chrome (ADR-004). Before, every entry was written as dead.
        let (_dir, file) = one_reference();
        let before = std::fs::read_to_string(&file).unwrap();
        let pool = Arc::new(
            BrowserPool::new(1, Some(&crate::profile::ProfileChoice::Fresh))
                .await
                .unwrap(),
        );
        pool.kill_chrome().await;

        let plan = Plan::load(&file, None).await.unwrap();
        let err = plan
            .run(&pool, 1, 5000, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("http://127.0.0.1:9/: Chrome failed"), "{err}");
        assert!(err.contains("was not changed"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    #[tokio::test]
    async fn chrome_killed_mid_navigation_changes_nothing_in_the_file() {
        // Needs Chrome (ADR-004). Before, the page's failure was written as dead.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("references.yaml");
        let pool = Arc::new(
            BrowserPool::new(1, Some(&crate::profile::ProfileChoice::Fresh))
                .await
                .unwrap(),
        );
        write_reference(
            &file,
            &crate::browser::tests::serve_trap(&pool, "-KILL").await,
        );
        let before = std::fs::read_to_string(&file).unwrap();

        let plan = Plan::load(&file, None).await.unwrap();
        let err = plan.run(&pool, 1, 10_000, false).await.unwrap_err();
        assert!(err.to_string().contains("Chrome failed"), "{err:#}");
        assert!(err.to_string().contains("was not changed"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        pool.stop().await.unwrap();
    }

    #[tokio::test]
    async fn nothing_chosen_needs_no_browser() {
        let (_dir, file) = one_reference();
        let plan = Plan::load(&file, Some(&["other".to_string()]))
            .await
            .unwrap();
        assert!(plan.is_empty());
        let output = plan.nothing_to_verify();
        assert_eq!(output.summary.total, 1);
        assert_eq!(output.summary.skipped, 1);
        assert_eq!(output.summary.verified, 0);
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
