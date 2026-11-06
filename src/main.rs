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

#[derive(clap::Args)]
struct McpArgs {
    /// Folder the file tools (`ref_pdf`, `ref_scan`, `ref_verify_refs`, `ref_refresh_data`)
    /// may read and write in [default: the current directory]
    #[arg(long)]
    root: Option<std::path::PathBuf>,
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
    Mcp(McpArgs),
}

/// Exit codes: 0 = everything succeeded, 1 = some URLs or files failed,
/// 2 = the command itself failed (bad arguments, Chrome missing, I/O error).
fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error: cannot start the async runtime: {e}");
            return ExitCode::from(2);
        }
    };
    let code = runtime.block_on(run(cli));
    // Shutting the runtime down drops every task, which kills Chrome; only
    // then can its temporary profile be removed for good.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    mollendorff_ref::browser::remove_open_profiles();
    code
}

async fn run(cli: Cli) -> ExitCode {
    let command = async move {
        match cli.command {
            Commands::Fetch(args) => run_fetch(args).await,
            Commands::Pdf(args) => run_pdf(&args).await,
            Commands::Init(args) => run_init(&args).map(|()| true),
            Commands::Scan(args) => run_scan(args).await.map(|()| true),
            Commands::CheckLinks(args) => run_check_links(args).await,
            Commands::RefreshData(args) => run_refresh_data(args).await,
            Commands::VerifyRefs(args) => run_verify_refs(args).await,
            Commands::Update(args) => run_update(args).await.map(|()| true),
            Commands::Mcp(args) => mollendorff_ref::mcp::run_mcp(args.root)
                .await
                .map(|()| true),
        }
    };

    // On Ctrl-C or SIGTERM the command is dropped, which closes Chrome and
    // removes its temporary profile, instead of leaving them behind.
    let result = tokio::select! {
        r = command => r,
        () = shutdown_signal() => {
            eprintln!("Interrupted.");
            return ExitCode::from(130);
        }
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

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
