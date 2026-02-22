//! Möllendorff Ref CLI
//!
//! LLM-optimized reference tools with headless Chrome.
//! Bypasses bot protection (403/999) that blocks curl/wget.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod browser;
mod check_links;
mod extract;
mod fetch;
mod init;
mod mcp;
mod pdf;
mod refresh_data;
mod scan;
mod schema;
mod update;
mod verify_refs;

use check_links::{run_check_links, CheckLinksArgs};
use fetch::{run_fetch, FetchArgs};
use init::{run_init, InitArgs};
use pdf::{run_pdf, PdfArgs};
use refresh_data::{run_refresh_data, RefreshDataArgs};
use scan::{run_scan, ScanArgs};
use update::{run_update, UpdateArgs};
use verify_refs::{run_verify_refs, VerifyRefsArgs};

#[derive(Parser)]
#[command(name = "ref")]
#[command(author = "Möllendorff AI")]
#[command(version)]
#[command(about = "Möllendorff Ref - LLM-optimized reference toolkit")]
#[command(
    long_about = "Reference verification and web fetching for AI agents.\nBypasses bot protection (403/999) that blocks curl/wget.\nAll output is JSON for LLM consumption."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Fetch URL and convert HTML to structured JSON (LLM-optimized)
    Fetch(FetchArgs),
    /// Extract text from PDF files to structured JSON
    Pdf(PdfArgs),
    /// Create references.yaml template
    Init(InitArgs),
    /// Scan markdown files for URLs, build references.yaml
    Scan(ScanArgs),
    /// Verify references.yaml entries and update status
    VerifyRefs(VerifyRefsArgs),
    /// Check URL health in markdown files or single URLs
    CheckLinks(CheckLinksArgs),
    /// Extract live data from URLs (market sizes, pricing, statistics)
    RefreshData(RefreshDataArgs),
    /// Update to the latest version from GitHub releases
    Update(UpdateArgs),
    /// Start MCP server (Model Context Protocol) on stdio
    Mcp,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Fetch(args) => run_fetch(args).await,
        Commands::Pdf(args) => run_pdf(&args),
        Commands::Init(args) => run_init(args).await,
        Commands::Scan(args) => run_scan(args).await,
        Commands::CheckLinks(args) => run_check_links(args).await,
        Commands::RefreshData(args) => run_refresh_data(args).await,
        Commands::VerifyRefs(args) => run_verify_refs(args).await,
        Commands::Update(args) => run_update(args).await,
        Commands::Mcp => mcp::run_mcp().await,
    }
}
