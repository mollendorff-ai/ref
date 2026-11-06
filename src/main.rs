//! Möllendorff Ref CLI
//!
//! LLM-optimized reference tools with headless Chrome.
//! Renders pages in headless Chrome, including JavaScript-heavy sites.

use clap::{Parser, Subcommand};
use std::process::ExitCode;

use mollendorff_ref::check_links::{run_check_links, CheckLinksArgs};
use mollendorff_ref::fetch::{run_fetch, FetchArgs};
use mollendorff_ref::init::{run_init, InitArgs};
use mollendorff_ref::pdf::{run_pdf, PdfArgs};
use mollendorff_ref::refresh_data::{run_refresh_data, RefreshDataArgs};
use mollendorff_ref::scan::{run_scan, ScanArgs};
use mollendorff_ref::update::{run_update, UpdateArgs};
use mollendorff_ref::verify_refs::{run_verify_refs, VerifyRefsArgs};

#[derive(Parser)]
#[command(name = "ref")]
#[command(author = "Möllendorff AI")]
#[command(version)]
#[command(about = "Möllendorff Ref - LLM-optimized reference toolkit")]
#[command(
    long_about = "Reference verification and web fetching for AI agents.\nRenders pages in headless Chrome, including JavaScript-heavy sites.\nAll output is JSON for LLM consumption.\n\nExit codes: 0 = all succeeded, 1 = some URLs or files failed, 2 = command error."
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

/// Exit codes: 0 = everything succeeded, 1 = some URLs or files failed,
/// 2 = the command itself failed (bad arguments, Chrome missing, I/O error).
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Fetch(args) => run_fetch(args).await,
        Commands::Pdf(args) => run_pdf(&args).await,
        Commands::Init(args) => run_init(args).await.map(|()| true),
        Commands::Scan(args) => run_scan(args).await.map(|()| true),
        Commands::CheckLinks(args) => run_check_links(args).await,
        Commands::RefreshData(args) => run_refresh_data(args).await.map(|()| true),
        Commands::VerifyRefs(args) => run_verify_refs(args).await,
        Commands::Update(args) => run_update(args).await.map(|()| true),
        Commands::Mcp => mollendorff_ref::mcp::run_mcp().await.map(|()| true),
    };

    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::from(2)
        }
    }
}
