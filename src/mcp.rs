//! MCP server mode: JSON-RPC 2.0 over stdio
//!
//! Exposes ref tools via the Model Context Protocol.
//! Start with `ref mcp` — persistent browser pool across tool calls.
//!
//! See: ADR-001 for rationale, docs/mcp-integration.md for setup.

use crate::browser::BrowserPool;
use crate::check_links::{self, check_links_with};
use crate::fetch;
use crate::guard;
use crate::pdf;
use crate::profile::ProfileChoice;
use crate::refresh_data::refresh_data_with;
use crate::scan::{self, ScanArgs};
use crate::verify_refs::Plan;
use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorCode, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

// ============================================================================
// Parameter structs (auto-generate JSON schemas via schemars)
// ============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchParams {
    /// URLs to fetch (one or more)
    pub urls: Vec<String>,
    /// Pages fetched at once, 1-20 (default: 4)
    pub parallel: Option<usize>,
    /// Timeout per URL in milliseconds (default: 30000)
    pub timeout: Option<u64>,
    /// Skip content cleaning, return raw extracted text
    pub raw: Option<bool>,
    /// CSS selector to extract a specific element (skips content heuristics)
    pub selector: Option<String>,
    /// Maximum characters per section; longer sections are cut and marked
    /// truncated (default: 5000, 0 = no limit)
    pub max_chars: Option<usize>,
    /// Maximum sections per page (default: 50, 0 = no limit)
    pub max_sections: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PdfParams {
    /// PDF file paths inside the working directory, or http(s):// URLs
    pub files: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckLinksParams {
    /// URLs to check
    pub urls: Vec<String>,
    /// Parallel browser tabs 1-20 (default: 5)
    pub concurrency: Option<usize>,
    /// Timeout per URL in milliseconds (default: 15000)
    pub timeout: Option<u64>,
    /// Retry count on failure (default: 1)
    pub retries: Option<u8>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScanParams {
    /// File paths or glob patterns to scan for URLs (relative, inside the working directory)
    pub files: Vec<String>,
    /// Output file path inside the working directory (default: references.yaml)
    pub output: Option<String>,
    /// Merge with existing file (default: true)
    pub merge: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifyRefsParams {
    /// Path to references.yaml inside the working directory
    pub file: String,
    /// Parallel browser tabs (default: 4)
    pub parallel: Option<usize>,
    /// Filter by categories
    pub category: Option<Vec<String>>,
    /// Timeout per URL in milliseconds (default: 30000)
    pub timeout: Option<u64>,
    /// Dry run without writing changes
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RefreshDataParams {
    /// URL to extract data from
    pub url: Option<String>,
    /// Markdown file to extract URLs from
    pub file: Option<String>,
    /// Timeout per URL in milliseconds (default: 20000)
    pub timeout: Option<u64>,
}

// ============================================================================
// MCP Server
// ============================================================================

/// Largest `parallel` / `concurrency` value a call may ask for
const MAX_PARALLEL: usize = 20;

/// Tabs the shared MCP browser may hold open at once, across all calls, so
/// one call can use the largest `parallel` / `concurrency` value it may ask
/// for; calls running at the same time share these tabs.
const MCP_POOL_TABS: usize = MAX_PARALLEL;

#[derive(Clone)]
pub struct RefMcpServer {
    /// The browser every tool shares, started on first use and again when it stops answering
    pool: Arc<Mutex<Option<Arc<BrowserPool>>>>,
    root: PathBuf,
    /// `--profile` value every browser of this server runs with
    profile: Option<ProfileChoice>,
    tool_router: ToolRouter<RefMcpServer>,
}

impl RefMcpServer {
    fn new(root: PathBuf, profile: Option<ProfileChoice>) -> Self {
        Self {
            pool: Arc::new(Mutex::new(None)),
            root,
            profile,
            tool_router: Self::tool_router(),
        }
    }

    /// The shared browser: started on first use, and started again between
    /// calls when Chrome has failed (ADR-005 rule 5), so one crash or hang
    /// does not end the agent's web access.
    async fn browser(&self) -> std::result::Result<Arc<BrowserPool>, ErrorData> {
        let mut slot = self.pool.lock().await;
        if let Some(pool) = slot.as_ref() {
            if pool.is_alive().await {
                return Ok(Arc::clone(pool));
            }
            info!("Chrome failed; starting a new one");
            // Calls still running on it end with ChromeFailed; stopping works
            // while they hold it, and frees the profile for the new Chrome
            let _ = pool.stop().await;
        }
        *slot = None;
        let pool = BrowserPool::new(MCP_POOL_TABS, self.profile.as_ref())
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("Browser pool init failed: {e:#}"), None)
            })?;
        let pool = Arc::new(pool);
        *slot = Some(Arc::clone(&pool));
        Ok(pool)
    }

    /// The folder MCP tools may read and write in.
    ///
    /// Refused when it is the filesystem root, which would open every file.
    fn workspace_root(&self) -> std::result::Result<PathBuf, ErrorData> {
        if self.root.parent().is_none() {
            return Err(invalid(
                "File access is disabled: the MCP server's workspace is the filesystem root. \
                 Start `ref mcp` from a project folder or pass `--root <folder>`."
                    .to_string(),
            ));
        }
        Ok(self.root.clone())
    }
}

/// Check a `parallel` / `concurrency` value: default when absent, else 1 to 20.
fn parallelism(
    value: Option<usize>,
    default: usize,
    name: &str,
) -> std::result::Result<usize, ErrorData> {
    match value {
        None => Ok(default),
        Some(n) if (1..=MAX_PARALLEL).contains(&n) => Ok(n),
        Some(n) => Err(invalid(format!(
            "`{name}` must be between 1 and {MAX_PARALLEL} (got {n})"
        ))),
    }
}

#[allow(clippy::needless_pass_by_value)] // used as map_err callback
fn invalid(msg: String) -> ErrorData {
    ErrorData::new(ErrorCode::INVALID_PARAMS, msg, None)
}

fn to_mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(format!("{e:#}"), None)
}

/// A browser tool's error: for a Chrome failure, also that the next call
/// starts a new Chrome (ADR-005 rule 3)
#[allow(clippy::needless_pass_by_value)] // used as map_err callback
fn browser_err(e: anyhow::Error) -> ErrorData {
    if e.is::<crate::browser::ChromeFailed>() {
        ErrorData::internal_error(format!("{e}; the next call starts a new Chrome"), None)
    } else {
        to_mcp_err(e)
    }
}

#[allow(clippy::needless_pass_by_value)] // used as map_err callback
fn json_err(e: serde_json::Error) -> ErrorData {
    ErrorData::internal_error(format!("JSON serialization failed: {e}"), None)
}

#[tool_router]
impl RefMcpServer {
    /// Render web pages in headless Chrome and return structured JSON: sections (lists and tables as Markdown), links, code blocks, HTTP status and a truncated flag.
    #[tool(name = "ref_fetch")]
    async fn ref_fetch(
        &self,
        Parameters(params): Parameters<FetchParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let parallel = parallelism(params.parallel, 4, "parallel")?;
        let options = fetch::FetchOptions {
            timeout: params.timeout.unwrap_or(30_000),
            raw: params.raw.unwrap_or(false),
            selector: params.selector,
            max_chars: params.max_chars.unwrap_or(fetch::DEFAULT_MAX_CHARS),
            max_sections: params
                .max_sections
                .unwrap_or(crate::content::DEFAULT_MAX_SECTIONS),
        };

        // Nothing to fetch: answer without starting Chrome
        if params.urls.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text("[]")]));
        }
        let shared = self.browser().await?;

        // Up to `parallel` pages at once, results in the order the URLs were given
        let (pool, options) = (&*shared, &options);
        let pages: Vec<fetch::Page> = futures::stream::iter(params.urls)
            .map(|url| async move { fetch::fetch_one(pool, &url, options).await })
            .buffered(parallel)
            .try_collect()
            .await
            .map_err(browser_err)?;

        let json = if pages.len() == 1 {
            serde_json::to_string(&pages[0])
        } else {
            serde_json::to_string(&pages)
        }
        .map_err(json_err)?;

        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Extract text, tables, and headings from PDF files or URLs.
    #[tool(name = "ref_pdf")]
    async fn ref_pdf(
        &self,
        Parameters(params): Parameters<PdfParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let root = self.workspace_root()?;
        let mut results = Vec::new();
        for file in &params.files {
            let page = if file.starts_with("http://") || file.starts_with("https://") {
                pdf::extract_pdf_from_url(file).await
            } else {
                let path = guard::check_workspace_path(file, &root).map_err(invalid)?;
                pdf::extract_pdf(&path)
            };
            results.push(page);
        }

        let json = if results.len() == 1 {
            serde_json::to_string(&results[0])
        } else {
            serde_json::to_string(&results)
        }
        .map_err(json_err)?;

        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Validate URL health using headless Chrome (returns status codes and redirect info).
    #[tool(name = "ref_check_links")]
    async fn ref_check_links(
        &self,
        Parameters(params): Parameters<CheckLinksParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let concurrency = parallelism(params.concurrency, 5, "concurrency")?;
        let report = if params.urls.is_empty() {
            check_links::LinkReport::default()
        } else {
            let pool = self.browser().await?;
            check_links_with(
                &pool,
                &params.urls,
                concurrency,
                params.timeout.unwrap_or(15000),
                params.retries.unwrap_or(1),
            )
            .await
            .map_err(browser_err)?
        };
        let json = serde_json::to_string(&report).map_err(json_err)?;

        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Scan markdown files for URLs and build/update references.yaml.
    #[tool(name = "ref_scan")]
    async fn ref_scan(
        &self,
        Parameters(params): Parameters<ScanParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let root = self.workspace_root()?;
        let mut patterns = Vec::new();
        for pattern in &params.files {
            guard::check_workspace_pattern(pattern).map_err(invalid)?;
            patterns.push(root.join(pattern));
        }
        // Expand globs here so every matched file is checked, including
        // files reached through a symlinked folder
        let mut files = Vec::new();
        for path in scan::expand_files(&patterns).await.map_err(to_mcp_err)? {
            let checked =
                guard::check_workspace_path(&path.to_string_lossy(), &root).map_err(invalid)?;
            files.push(checked);
        }
        let output = params
            .output
            .unwrap_or_else(|| "references.yaml".to_string());
        let output = guard::check_workspace_path(&output, &root).map_err(invalid)?;
        // A refused output is a bad parameter, like a path outside the workspace
        // (scan_files checks again for the CLI)
        guard::check_references_output(&output).map_err(invalid)?;
        let args = ScanArgs {
            files,
            output,
            merge: params.merge.unwrap_or(true),
            merge_compat: false,
        };

        let output = scan::scan_files(args).await.map_err(to_mcp_err)?;
        let json = serde_json::to_string(&output).map_err(json_err)?;

        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Verify references.yaml entries by fetching each URL and updating status.
    #[tool(name = "ref_verify_refs")]
    async fn ref_verify_refs(
        &self,
        Parameters(params): Parameters<VerifyRefsParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let root = self.workspace_root()?;
        let file = guard::check_workspace_path(&params.file, &root).map_err(invalid)?;
        let parallel = parallelism(params.parallel, 4, "parallel")?;
        // The file is read and checked before Chrome is started
        let plan = Plan::load(&file, params.category.as_deref())
            .await
            .map_err(to_mcp_err)?;
        let output = if plan.is_empty() {
            plan.nothing_to_verify()
        } else {
            let pool = self.browser().await?;
            let timeout = params.timeout.unwrap_or(30000);
            plan.run(&pool, parallel, timeout, params.dry_run.unwrap_or(false))
                .await
                .map_err(browser_err)?
        };
        let json = serde_json::to_string(&output).map_err(json_err)?;

        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Extract live data (amounts, percentages, statistics) from URLs.
    #[tool(name = "ref_refresh_data")]
    async fn ref_refresh_data(
        &self,
        Parameters(params): Parameters<RefreshDataParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        if params.url.is_none() && params.file.is_none() {
            return Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "Either 'url' or 'file' is required",
                None,
            ));
        }

        let timeout_ms = params.timeout.unwrap_or(20000);

        let urls: Vec<String> = if let Some(url) = params.url {
            vec![url]
        } else {
            let file = params.file.unwrap_or_default();
            let path =
                guard::check_workspace_path(&file, &self.workspace_root()?).map_err(invalid)?;
            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                ErrorData::internal_error(format!("Failed to read {file}: {e}"), None)
            })?;
            crate::extract::extract_urls(&content)
        };

        let report = if urls.is_empty() {
            crate::refresh_data::RefreshReport::default()
        } else {
            let pool = self.browser().await?;
            refresh_data_with(&pool, &urls, timeout_ms)
                .await
                .map_err(browser_err)?
        };
        let json = serde_json::to_string(&report).map_err(json_err)?;

        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }
}

// The macro generates async fns without `.await`; clippy 1.99 flags them.
#[allow(unknown_lints)]
#[allow(clippy::unused_async_trait_impl)]
#[tool_handler(router = self.tool_router)]
impl rmcp::handler::server::ServerHandler for RefMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ref", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Möllendorff Ref: compact JSON from web pages and PDFs for AI agents. \
                 Renders JavaScript-heavy pages in headless Chrome. \
                 Each result has a status (ok, dead, redirect, paywall, login) and an http code; \
                 check `truncated` before treating content as complete.",
            )
    }
}

/// Start the MCP server on stdio.
///
/// `profile` is the `--profile` value its browsers run with (see [`crate::profile`]).
///
/// # Errors
///
/// Returns an error if the transport fails to initialize or the server exits abnormally.
pub async fn run_mcp(root: Option<PathBuf>, profile: Option<ProfileChoice>) -> Result<()> {
    // All log output to stderr (stdout reserved for JSON-RPC)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("info".parse().unwrap_or_default()),
        )
        .with_ansi(false)
        .init();

    info!("Starting ref MCP server v{}", env!("CARGO_PKG_VERSION"));

    let root = match root {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let root = root
        .canonicalize()
        .with_context(|| format!("Workspace folder {} is not available", root.display()))?;
    info!("Workspace for file tools: {}", root.display());

    let server = RefMcpServer::new(root, profile);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;

    info!("MCP server stopped");
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::ServerHandler;

    #[test]
    fn test_fetch_params_minimal() {
        let json = r#"{"urls": ["https://example.com"]}"#;
        let params: FetchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.urls.len(), 1);
        assert!(params.parallel.is_none());
        assert!(params.timeout.is_none());
        assert!(params.raw.is_none());
    }

    #[test]
    fn test_fetch_params_full() {
        let json = r#"{"urls": ["https://a.com", "https://b.com"], "parallel": 2, "timeout": 5000, "raw": true}"#;
        let params: FetchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.urls.len(), 2);
        assert_eq!(params.parallel, Some(2));
        assert_eq!(params.timeout, Some(5000));
        assert_eq!(params.raw, Some(true));
    }

    #[test]
    fn test_pdf_params() {
        let json = r#"{"files": ["/tmp/test.pdf"]}"#;
        let params: PdfParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.files, vec!["/tmp/test.pdf"]);
    }

    #[test]
    fn test_check_links_params_defaults() {
        let json = r#"{"urls": ["https://example.com"]}"#;
        let params: CheckLinksParams = serde_json::from_str(json).unwrap();
        assert!(params.concurrency.is_none());
        assert!(params.timeout.is_none());
        assert!(params.retries.is_none());
    }

    #[test]
    fn test_scan_params() {
        let json = r#"{"files": ["docs/*.md"], "output": "refs.yaml"}"#;
        let params: ScanParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.files, vec!["docs/*.md"]);
        assert_eq!(params.output, Some("refs.yaml".to_string()));
        assert!(params.merge.is_none());
    }

    #[test]
    fn test_verify_refs_params() {
        let json = r#"{"file": "references.yaml", "parallel": 8, "dry_run": true}"#;
        let params: VerifyRefsParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.file, "references.yaml");
        assert_eq!(params.parallel, Some(8));
        assert_eq!(params.dry_run, Some(true));
        assert!(params.category.is_none());
    }

    #[test]
    fn test_refresh_data_params_url() {
        let json = r#"{"url": "https://example.com/stats/123"}"#;
        let params: RefreshDataParams = serde_json::from_str(json).unwrap();
        assert!(params.url.is_some());
        assert!(params.file.is_none());
    }

    #[test]
    fn test_refresh_data_params_file() {
        let json = r#"{"file": "data.md", "timeout": 10000}"#;
        let params: RefreshDataParams = serde_json::from_str(json).unwrap();
        assert!(params.url.is_none());
        assert_eq!(params.file, Some("data.md".to_string()));
        assert_eq!(params.timeout, Some(10000));
    }

    #[test]
    fn test_server_info() {
        let server = RefMcpServer::new(std::env::current_dir().unwrap(), None);
        let info = server.get_info();
        assert_eq!(info.server_info.name, "ref");
        assert!(info.capabilities.tools.is_some());
    }

    #[test]
    fn test_tools_list_all_registered() {
        let server = RefMcpServer::new(std::env::current_dir().unwrap(), None);
        let tools = server.tool_router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();

        assert_eq!(tools.len(), 6, "Expected 6 tools, got: {names:?}");
        assert!(names.contains(&"ref_fetch"));
        assert!(names.contains(&"ref_pdf"));
        assert!(names.contains(&"ref_check_links"));
        assert!(names.contains(&"ref_scan"));
        assert!(names.contains(&"ref_verify_refs"));
        assert!(names.contains(&"ref_refresh_data"));
    }

    #[test]
    fn test_tools_have_descriptions() {
        let server = RefMcpServer::new(std::env::current_dir().unwrap(), None);
        let tools = server.tool_router.list_all();

        for tool in &tools {
            assert!(
                tool.description.is_some(),
                "Tool '{}' missing description",
                tool.name
            );
            assert!(
                !tool.description.as_ref().unwrap().is_empty(),
                "Tool '{}' has empty description",
                tool.name
            );
        }
    }

    #[tokio::test]
    async fn test_ref_pdf_refuses_paths_outside_workdir() {
        let server = RefMcpServer::new(std::env::current_dir().unwrap(), None);
        for file in ["/etc/hostname", "../outside.pdf"] {
            let params = PdfParams {
                files: vec![file.to_string()],
            };
            let err = server.ref_pdf(Parameters(params)).await.unwrap_err();
            assert!(err.message.contains("working directory"), "{}", err.message);
        }
    }

    #[tokio::test]
    async fn test_ref_scan_refused_output_is_invalid_params() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "see https://example.com").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a references file").unwrap();
        std::fs::write(dir.path().join("other.yaml"), "just: some yaml").unwrap();
        let server = RefMcpServer::new(dir.path().canonicalize().unwrap(), None);
        // not .yaml/.yml, and an existing file that is not a references file
        for output in ["a.md", "notes.txt", "other.yaml"] {
            let params = ScanParams {
                files: vec!["a.md".to_string()],
                output: Some(output.to_string()),
                merge: None,
            };
            let err = server.ref_scan(Parameters(params)).await.unwrap_err();
            assert_eq!(
                err.code,
                ErrorCode::INVALID_PARAMS,
                "{output}: {}",
                err.message
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("other.yaml")).unwrap(),
            "just: some yaml"
        );
    }

    fn text(result: &CallToolResult) -> serde_json::Value {
        let t = result.content[0].as_text().map(|t| t.text.clone());
        serde_json::from_str(&t.unwrap_or_default()).unwrap()
    }

    #[tokio::test]
    async fn calls_with_nothing_to_do_never_start_chrome() {
        // The profile is a file, so starting Chrome would fail the call
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let not_a_folder = root.join("not-a-folder");
        std::fs::write(&not_a_folder, "").unwrap();
        crate::verify_refs::tests::write_one_reference(&root.join("references.yaml"));
        std::fs::write(root.join("empty.md"), "No links here.").unwrap();
        let server = RefMcpServer::new(root, Some(ProfileChoice::Folder(not_a_folder)));

        let fetched = server
            .ref_fetch(Parameters(FetchParams {
                urls: vec![],
                parallel: None,
                timeout: None,
                raw: None,
                selector: None,
                max_chars: None,
                max_sections: None,
            }))
            .await
            .unwrap();
        assert_eq!(text(&fetched), serde_json::json!([]));

        let checked = server
            .ref_check_links(Parameters(CheckLinksParams {
                urls: vec![],
                concurrency: None,
                timeout: None,
                retries: None,
            }))
            .await
            .unwrap();
        assert_eq!(text(&checked)["ok"], 0);

        let refreshed = server
            .ref_refresh_data(Parameters(RefreshDataParams {
                url: None,
                file: Some("empty.md".into()),
                timeout: None,
            }))
            .await
            .unwrap();
        assert_eq!(text(&refreshed)["failed"], 0);

        let verify = |file: &str, category: Option<Vec<String>>| VerifyRefsParams {
            file: file.into(),
            parallel: None,
            category,
            timeout: None,
            dry_run: Some(true),
        };
        let filtered = server
            .ref_verify_refs(Parameters(verify(
                "references.yaml",
                Some(vec!["none".into()]),
            )))
            .await
            .unwrap();
        assert_eq!(text(&filtered)["summary"]["skipped"], 1);
        let missing = server
            .ref_verify_refs(Parameters(verify("missing.yaml", None)))
            .await
            .unwrap_err();
        assert!(
            missing.message.contains("Failed to read"),
            "{}",
            missing.message
        );

        assert!(server.pool.lock().await.is_none(), "Chrome was started");

        // With something to do, the unusable profile is reported as such
        let refused = server
            .ref_check_links(Parameters(CheckLinksParams {
                urls: vec!["http://127.0.0.1:9/".into()],
                concurrency: None,
                timeout: None,
                retries: None,
            }))
            .await
            .unwrap_err();
        assert!(
            refused.message.starts_with("Browser pool init failed"),
            "{}",
            refused.message
        );
    }

    #[tokio::test]
    async fn a_dead_chrome_is_replaced_on_the_next_call() {
        // Needs Chrome (ADR-004). A saved profile: the dead Chrome, not yet
        // collected by its parent, must not keep the new one off the profile.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let server = RefMcpServer::new(
            root.clone(),
            Some(ProfileChoice::Folder(root.join("saved"))),
        );
        let check = || CheckLinksParams {
            urls: vec!["http://127.0.0.1:9/".into()],
            concurrency: None,
            timeout: Some(5000),
            retries: Some(0),
        };

        server.ref_check_links(Parameters(check())).await.unwrap();
        let first = server.pool.lock().await.clone().unwrap();
        first.kill_chrome().await;

        let again = server.ref_check_links(Parameters(check())).await.unwrap();
        let error = text(&again)["results"][0]["error"].to_string();
        assert!(
            !error.contains("Browser error"),
            "the dead Chrome was used: {error}"
        );
        let second = server.pool.lock().await.clone().unwrap();
        assert!(!Arc::ptr_eq(&first, &second), "no new Chrome was started");
        assert!(second.is_alive().await);
    }

    #[test]
    fn only_a_chrome_failure_promises_a_new_chrome() {
        let page = browser_err(anyhow::anyhow!("Failed to write references.yaml"));
        assert_eq!(page.message, "Failed to write references.yaml");
        let chrome = browser_err(anyhow::Error::new(crate::browser::ChromeFailed(
            "gone".into(),
        )));
        assert_eq!(
            chrome.message,
            "Chrome failed: gone; the next call starts a new Chrome"
        );
    }

    #[tokio::test]
    async fn rule_5_a_frozen_chrome_is_replaced_and_the_call_on_it_says_so() {
        // Needs Chrome (ADR-004)
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let server = RefMcpServer::new(
            root.clone(),
            Some(ProfileChoice::Folder(root.join("saved"))),
        );
        let fetch = |url: String| FetchParams {
            urls: vec![url],
            parallel: None,
            timeout: Some(2000),
            raw: None,
            selector: None,
            max_chars: None,
            max_sections: None,
        };

        let site = crate::browser::tests::serve_cookie_jar();
        server
            .ref_fetch(Parameters(fetch(format!("{site}/show"))))
            .await
            .unwrap();
        let first = server.pool.lock().await.clone().unwrap();
        let trap = crate::browser::tests::serve_trap(&first, "-STOP").await;

        // The call during the failure gets the tool error
        let failed = server.ref_fetch(Parameters(fetch(trap))).await.unwrap_err();
        assert!(
            failed.message.contains("Chrome failed"),
            "{}",
            failed.message
        );
        assert!(
            failed.message.contains("next call starts a new Chrome"),
            "{}",
            failed.message
        );

        // The next call starts a new Chrome on the same saved profile
        let again = server
            .ref_fetch(Parameters(fetch(format!("{site}/show"))))
            .await;
        let again = again.unwrap();
        assert_eq!(text(&again)["status"], "ok");
        let second = server.pool.lock().await.clone().unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        second.stop().await.unwrap();
    }

    #[tokio::test]
    async fn browser_tools_share_one_chrome_on_a_saved_profile() {
        // Needs Chrome (ADR-004). Before the fix, each of these tools started a
        // second Chrome on the profile the server's browser already held, and
        // failed with "The profile ... is open in another Chrome".
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        crate::verify_refs::tests::write_one_reference(&root.join("references.yaml"));
        std::fs::write(
            root.join("notes.md"),
            "See http://127.0.0.1:9/ for nothing.",
        )
        .unwrap();
        let saved = root.join("saved-profile");
        let server = RefMcpServer::new(root.clone(), Some(ProfileChoice::Folder(saved.clone())));
        let text = |r: CallToolResult| text(&r);

        let fetched = server
            .ref_fetch(Parameters(FetchParams {
                urls: vec!["http://127.0.0.1:9/".into()],
                parallel: None,
                timeout: Some(5000),
                raw: None,
                selector: None,
                max_chars: None,
                max_sections: None,
            }))
            .await
            .unwrap();
        assert_eq!(text(fetched)["status"], "dead");
        assert_eq!(
            server.pool.lock().await.as_ref().unwrap().profile,
            crate::profile::Profile::Saved(saved.clone())
        );

        let checked = server
            .ref_check_links(Parameters(CheckLinksParams {
                urls: vec!["http://127.0.0.1:9/".into()],
                concurrency: None,
                timeout: Some(5000),
                retries: Some(0),
            }))
            .await
            .unwrap();
        assert_eq!(text(checked)["failed"], 1);

        let refreshed = server
            .ref_refresh_data(Parameters(RefreshDataParams {
                url: None,
                file: Some("notes.md".into()),
                timeout: Some(5000),
            }))
            .await
            .unwrap();
        assert_eq!(text(refreshed)["failed"], 1);

        let verified = server
            .ref_verify_refs(Parameters(VerifyRefsParams {
                file: "references.yaml".into(),
                parallel: None,
                category: None,
                timeout: Some(5000),
                dry_run: Some(true),
            }))
            .await
            .unwrap();
        let verified = text(verified);
        assert_eq!(verified["summary"]["verified"], 1);
        assert_eq!(verified["summary"]["dead"], 1);
        assert!(saved.is_dir(), "the saved profile is kept");
    }

    #[tokio::test]
    async fn test_ref_pdf_tool_call() {
        let server = RefMcpServer::new(std::env::current_dir().unwrap(), None);

        // Call with a non-existent file — should return success with error in JSON body
        // (tool-level errors are returned as JSON in CallToolResult, not MCP errors)
        let params = PdfParams {
            files: vec!["nonexistent_test_file.pdf".to_string()],
        };
        let result = server.ref_pdf(Parameters(params)).await;

        assert!(
            result.is_ok(),
            "Tool should not return MCP error for missing file"
        );
        let call_result = result.unwrap();

        // Verify the result contains JSON with error status
        let content = &call_result.content[0];
        let text = match content {
            ContentBlock::Text(t) => &t.text,
            _ => panic!("Expected text content"),
        };
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            parsed["status"], "dead",
            "Missing file should produce dead status"
        );
    }
}
