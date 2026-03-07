//! MCP server mode: JSON-RPC 2.0 over stdio
//!
//! Exposes ref tools via the Model Context Protocol.
//! Start with `ref mcp` — persistent browser pool across tool calls.
//!
//! See: ADR-001 for rationale, docs/mcp-integration.md for setup.

use crate::browser::BrowserPool;
use crate::check_links::{check_links, CheckLinksConfig};
use crate::fetch;
use crate::pdf;
use crate::refresh_data::{refresh_data, RefreshConfig};
use crate::scan::{self, ScanArgs};
use crate::verify_refs::{self, VerifyRefsArgs};
use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, ErrorCode, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tracing::info;

// ============================================================================
// Parameter structs (auto-generate JSON schemas via schemars)
// ============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchParams {
    /// URLs to fetch (one or more)
    pub urls: Vec<String>,
    /// Max parallel browser tabs (default: 4)
    pub parallel: Option<usize>,
    /// Timeout per URL in milliseconds (default: 30000)
    pub timeout: Option<u64>,
    /// Skip content cleaning, return raw extracted text
    pub raw: Option<bool>,
    /// CSS selector to extract a specific element (skips content heuristics)
    pub selector: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PdfParams {
    /// Local file paths or https:// URLs to PDF files
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
    /// File paths or glob patterns to scan for URLs
    pub files: Vec<String>,
    /// Output file path (default: references.yaml)
    pub output: Option<String>,
    /// Merge with existing file (default: true)
    pub merge: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VerifyRefsParams {
    /// Path to references.yaml file
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

#[derive(Clone)]
pub struct RefMcpServer {
    pool: Arc<OnceCell<BrowserPool>>,
    tool_router: ToolRouter<RefMcpServer>,
}

impl RefMcpServer {
    fn new() -> Self {
        Self {
            pool: Arc::new(OnceCell::new()),
            tool_router: Self::tool_router(),
        }
    }

    async fn get_or_init_pool(
        &self,
        concurrency: usize,
    ) -> std::result::Result<&BrowserPool, ErrorData> {
        self.pool
            .get_or_try_init(|| async { BrowserPool::new(concurrency).await })
            .await
            .map_err(|e| ErrorData::internal_error(format!("Browser pool init failed: {e}"), None))
    }
}

fn to_mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(format!("{e:#}"), None)
}

#[allow(clippy::needless_pass_by_value)] // used as map_err callback
fn json_err(e: serde_json::Error) -> ErrorData {
    ErrorData::internal_error(format!("JSON serialization failed: {e}"), None)
}

#[tool_router]
impl RefMcpServer {
    /// Render web pages via headless Chrome and return structured JSON with sections, links, and code blocks.
    #[tool(name = "ref_fetch")]
    async fn ref_fetch(
        &self,
        Parameters(params): Parameters<FetchParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let parallel = params.parallel.unwrap_or(4).min(params.urls.len()).max(1);
        let timeout = params.timeout.unwrap_or(30000);
        let raw = params.raw.unwrap_or(false);

        let pool = self.get_or_init_pool(parallel).await?;

        let selector = params.selector.as_deref();

        let mut pages = Vec::new();
        for url in &params.urls {
            let page = fetch::fetch_one(pool, url, timeout, raw, selector).await;
            pages.push(page);
        }

        let json = if pages.len() == 1 {
            serde_json::to_string(&pages[0])
        } else {
            serde_json::to_string(&pages)
        }
        .map_err(json_err)?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Extract text, tables, and headings from PDF files or URLs.
    #[tool(name = "ref_pdf")]
    async fn ref_pdf(
        &self,
        Parameters(params): Parameters<PdfParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let mut results = Vec::new();
        for file in &params.files {
            let page = if file.starts_with("http://") || file.starts_with("https://") {
                pdf::extract_pdf_from_url(file).await
            } else {
                pdf::extract_pdf(&PathBuf::from(file))
            };
            results.push(page);
        }

        let json = if results.len() == 1 {
            serde_json::to_string(&results[0])
        } else {
            serde_json::to_string(&results)
        }
        .map_err(json_err)?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Validate URL health using headless Chrome (returns status codes and redirect info).
    #[tool(name = "ref_check_links")]
    async fn ref_check_links(
        &self,
        Parameters(params): Parameters<CheckLinksParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let config = CheckLinksConfig {
            concurrency: params.concurrency.unwrap_or(5),
            timeout_ms: params.timeout.unwrap_or(15000),
            retries: params.retries.unwrap_or(1),
        };

        let report = check_links(&params.urls, &config)
            .await
            .map_err(to_mcp_err)?;
        let json = serde_json::to_string(&report).map_err(json_err)?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Scan markdown files for URLs and build/update references.yaml.
    #[tool(name = "ref_scan")]
    async fn ref_scan(
        &self,
        Parameters(params): Parameters<ScanParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let args = ScanArgs {
            files: params.files.into_iter().map(PathBuf::from).collect(),
            output: PathBuf::from(
                params
                    .output
                    .unwrap_or_else(|| "references.yaml".to_string()),
            ),
            merge: params.merge.unwrap_or(true),
        };

        let output = scan::scan_files(args).await.map_err(to_mcp_err)?;
        let json = serde_json::to_string(&output).map_err(json_err)?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Verify references.yaml entries by fetching each URL and updating status.
    #[tool(name = "ref_verify_refs")]
    async fn ref_verify_refs(
        &self,
        Parameters(params): Parameters<VerifyRefsParams>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let args = VerifyRefsArgs {
            file: PathBuf::from(params.file),
            parallel: params.parallel.unwrap_or(4),
            category: params.category,
            timeout: params.timeout.unwrap_or(30000),
            dry_run: params.dry_run.unwrap_or(false),
        };

        let output = verify_refs::verify_refs_core(args)
            .await
            .map_err(to_mcp_err)?;
        let json = serde_json::to_string(&output).map_err(json_err)?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
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

        let config = RefreshConfig {
            timeout_ms: params.timeout.unwrap_or(20000),
        };

        let urls: Vec<(String, String)> = if let Some(url) = params.url {
            let ext_type = infer_extractor_type(&url);
            vec![(url, ext_type.to_string())]
        } else {
            let file = params.file.unwrap();
            let content = tokio::fs::read_to_string(&file).await.map_err(|e| {
                ErrorData::internal_error(format!("Failed to read {file}: {e}"), None)
            })?;
            crate::extract::extract_urls(&content)
                .into_iter()
                .map(|u| {
                    let t = infer_extractor_type(&u);
                    (u, t.to_string())
                })
                .collect()
        };

        let report = refresh_data(&urls, &config).await.map_err(to_mcp_err)?;
        let json = serde_json::to_string(&report).map_err(json_err)?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl rmcp::handler::server::ServerHandler for RefMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: rmcp::model::ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "ref".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            instructions: Some(
                "Möllendorff Ref: LLM-optimized web fetching and PDF extraction. \
                 Renders pages via headless Chrome, bypasses bot protection."
                    .into(),
            ),
        }
    }
}

/// Auto-detect extractor type from URL (mirrors `refresh_data::get_extractor_type`).
fn infer_extractor_type(url: &str) -> &'static str {
    if url.contains("instagram.com") {
        "instagram"
    } else if url.contains("statista.com") {
        "statista"
    } else {
        "generic"
    }
}

/// Start the MCP server on stdio.
///
/// # Errors
///
/// Returns an error if the transport fails to initialize or the server exits abnormally.
pub async fn run_mcp() -> Result<()> {
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

    let server = RefMcpServer::new();
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
        let json = r#"{"url": "https://statista.com/stats/123"}"#;
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
        let server = RefMcpServer::new();
        let info = server.get_info();
        assert_eq!(info.server_info.name, "ref");
        assert!(info.capabilities.tools.is_some());
    }

    #[test]
    fn test_tools_list_all_registered() {
        let server = RefMcpServer::new();
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
        let server = RefMcpServer::new();
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
    async fn test_ref_pdf_tool_call() {
        let server = RefMcpServer::new();

        // Call with a non-existent file — should return success with error in JSON body
        // (tool-level errors are returned as JSON in CallToolResult, not MCP errors)
        let params = PdfParams {
            files: vec!["/tmp/nonexistent_test_file.pdf".to_string()],
        };
        let result = server.ref_pdf(Parameters(params)).await;

        assert!(
            result.is_ok(),
            "Tool should not return MCP error for missing file"
        );
        let call_result = result.unwrap();

        // Verify the result contains JSON with error status
        let content = &call_result.content[0];
        let text = match &content.raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("Expected text content"),
        };
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            parsed["status"], "dead",
            "Missing file should produce dead status"
        );
    }
}
