//! The command line: the arguments, the dispatch to each command, and the
//! exit codes. `main` only calls [`main`], so all of it can be tested in-process.

use crate::check_links::{run_check_links, CheckLinksArgs};
use crate::fetch::{run_fetch, FetchArgs};
use crate::init::{run_init, InitArgs};
use crate::login::{run_login, LoginArgs};
use crate::pdf::{run_pdf, PdfArgs};
use crate::refresh_data::{run_refresh_data, RefreshDataArgs};
use crate::scan::{run_scan, ScanArgs};
use crate::update::{run_update, UpdateArgs};
use crate::verify_refs::{run_verify_refs, VerifyRefsArgs};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::future::Future;
use std::io::Write;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "ref")]
#[command(author = "Möllendorff AI")]
#[command(version)]
#[command(about = "Möllendorff Ref - LLM-optimized reference toolkit")]
#[command(
    long_about = "Reference verification and web fetching for AI agents.\nRenders pages in headless Chrome, including JavaScript-heavy sites.\nAll output is JSON for LLM consumption.\n\nExit codes: 0 = all succeeded, 1 = some URLs or files failed, 2 = command error."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(clap::Args)]
pub struct McpArgs {
    /// Folder the file tools (`ref_pdf`, `ref_scan`, `ref_verify_refs`, `ref_refresh_data`)
    /// may read and write in [default: the current directory]
    #[arg(long)]
    pub root: Option<std::path::PathBuf>,

    #[command(flatten)]
    pub profile: crate::profile::ProfileArgs,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Fetch URL and convert HTML to structured JSON (LLM-optimized)
    Fetch(FetchArgs),
    /// Extract text from PDF files to structured JSON
    Pdf(PdfArgs),
    /// Create references.yaml template
    Init(InitArgs),
    /// Open Chrome with ref's saved profile to sign in to your sites once
    Login(LoginArgs),
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

/// Run the chosen command, writing its JSON to `out`: `true` when everything
/// succeeded, `false` when some URLs or files failed.
///
/// # Errors
/// Returns the command's error (bad arguments, Chrome missing, I/O error).
pub async fn run(cli: Cli, out: &mut impl Write) -> Result<bool> {
    match cli.command {
        Commands::Fetch(args) => run_fetch(args, out).await,
        Commands::Pdf(args) => run_pdf(&args, out).await,
        Commands::Init(args) => run_init(&args, out).map(|()| true),
        Commands::Login(args) => run_login(&args, out).map(|()| true),
        Commands::Scan(args) => run_scan(args, out).await.map(|()| true),
        Commands::CheckLinks(args) => run_check_links(args, out).await,
        Commands::RefreshData(args) => run_refresh_data(args, out).await,
        Commands::VerifyRefs(args) => run_verify_refs(args, out).await,
        Commands::Update(args) => run_update(args, out).await.map(|()| true),
        Commands::Mcp(args) => {
            let profile = args.profile.choice()?;
            crate::mcp::run_mcp(args.root, profile).await.map(|()| true)
        }
    }
}

/// Exit codes: 0 = everything succeeded, 1 = some URLs or files failed,
/// 2 = the command itself failed (bad arguments, Chrome missing, I/O error),
/// 130 = interrupted.
///
/// `args` are the command line (first the program name); the command's JSON
/// goes to `out` and its error to `err`. When `stop` finishes first (Ctrl-C or
/// SIGTERM in the real program), the command is dropped, which closes Chrome
/// and removes its temporary profile, instead of leaving them behind.
pub async fn execute<I, T>(
    args: I,
    out: &mut impl Write,
    err: &mut impl Write,
    stop: impl Future<Output = ()>,
) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // --help and --version go to the output, usage errors to the error stream
            let shown = if e.use_stderr() {
                write!(err, "{e}")
            } else {
                write!(out, "{e}")
            };
            let _ = shown;
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let result = tokio::select! {
        r = run(cli, out) => r,
        () = stop => {
            let _ = writeln!(err, "Interrupted.");
            return ExitCode::from(130);
        }
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            let _ = writeln!(err, "Error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// The real program: the command line, standard output and error, and
/// Ctrl-C or SIGTERM as the stop signal.
#[must_use]
pub fn main() -> ExitCode {
    start(
        tokio::runtime::Runtime::new(),
        std::env::args_os(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

/// [`main`] with its parts given, so the runtime failing to start can be tested
fn start<I, T>(
    runtime: std::io::Result<tokio::runtime::Runtime>,
    args: I,
    out: &mut impl Write,
    err: &mut impl Write,
) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let runtime = match runtime {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(err, "Error: cannot start the async runtime: {e}");
            return ExitCode::from(2);
        }
    };
    let code = runtime.block_on(execute(args, out, err, shutdown_signal()));
    // Shutting the runtime down drops every task, which kills Chrome; only
    // then can its temporary profile be removed for good.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    crate::browser::remove_open_profiles();
    code
}

/// Finishes on Ctrl-C or SIGTERM
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `ref <args>` in-process: (exit code, standard output, error output)
    async fn ref_cmd(args: &[&str]) -> (ExitCode, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let line = std::iter::once("ref").chain(args.iter().copied());
        let code = execute(line, &mut out, &mut err, std::future::pending()).await;
        let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
        (code, text(out), text(err))
    }

    fn code(n: u8) -> ExitCode {
        ExitCode::from(n)
    }

    #[tokio::test]
    async fn help_and_version_go_to_the_output_with_exit_code_0() {
        let (c, out, _) = ref_cmd(&["--help"]).await;
        assert_eq!(c, ExitCode::SUCCESS);
        for word in ["check-links", "refresh-data", "update", "mcp"] {
            assert!(out.contains(word), "{word} missing from {out}");
        }
        let (c, out, _) = ref_cmd(&["--version"]).await;
        assert_eq!(c, ExitCode::SUCCESS);
        assert!(out.contains("ref"), "{out}");
    }

    #[tokio::test]
    async fn each_command_documents_its_options() {
        let cases: [(&[&str], &[&str]); 4] = [
            (
                &["check-links", "--help"],
                &["--concurrency", "--url", "--stdin"],
            ),
            (&["refresh-data", "--help"], &["--url", "--timeout"]),
            (
                &["verify-refs", "--help"],
                &["--parallel", "--category", "--dry-run"],
            ),
            (&["update", "--help"], &["--check", "--force"]),
        ];
        for (args, words) in cases {
            let (c, out, _) = ref_cmd(args).await;
            assert_eq!(c, ExitCode::SUCCESS, "{args:?}");
            for word in words {
                assert!(out.contains(word), "{word} missing from {out}");
            }
        }
    }

    #[tokio::test]
    async fn bad_arguments_exit_with_code_2_and_a_usage_message() {
        for args in [
            &["check-links", "--concurrency", "0", "x.md"][..],
            &["check-links", "--concurrency", "21", "x.md"][..],
            &["no-such-command"][..],
        ] {
            let (c, out, err) = ref_cmd(args).await;
            assert_eq!(c, code(2), "{args:?}");
            assert!(out.is_empty(), "{out}");
            assert!(err.starts_with("error:"), "{err}");
        }
    }

    #[tokio::test]
    async fn commands_without_what_they_need_say_how_to_use_them() {
        for (args, word) in [
            (&["check-links"][..], "ref check-links --url"),
            (&["refresh-data"][..], "ref refresh-data --url"),
        ] {
            let (c, _, err) = ref_cmd(args).await;
            assert_eq!(c, code(2), "{args:?}");
            assert!(err.starts_with("Error: Usage:"), "{err}");
            assert!(err.contains(word), "{err}");
        }
    }

    #[tokio::test]
    async fn a_missing_file_is_an_error_and_a_file_without_urls_is_a_failure() {
        let (c, _, err) = ref_cmd(&["check-links", "nonexistent.md"]).await;
        assert_eq!(c, code(2));
        assert!(err.contains("Failed to read file"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.md");
        std::fs::write(&file, "# No URLs here\n\nJust text.").unwrap();
        let file = file.to_str().unwrap();
        for command in ["check-links", "refresh-data"] {
            let (c, out, _) = ref_cmd(&[command, file]).await;
            assert_eq!(c, code(1), "{command}");
            assert!(out.is_empty(), "{out}");
        }
    }

    #[tokio::test]
    async fn profile_is_taken_only_by_the_commands_that_start_chrome() {
        let url = "https://example.com";
        let chrome: [&[&str]; 5] = [
            &["fetch", "--profile", "fresh", url],
            &["check-links", "--profile", "fresh", "--url", url],
            &["verify-refs", "--profile", "fresh", "references.yaml"],
            &["refresh-data", "--profile", "fresh", "--url", url],
            &["mcp", "--profile", "fresh"],
        ];
        for args in chrome {
            let argv = std::iter::once("ref").chain(args.iter().copied());
            assert!(Cli::try_parse_from(argv).is_ok(), "{args:?}");
        }
        // `pdf` downloads without Chrome; `login` has its own --profile <folder>
        for args in [
            &["pdf", "--profile", "fresh", "a.pdf"][..],
            &["init", "--profile", "fresh"][..],
            &["scan", "--profile", "fresh", "a.md"][..],
        ] {
            let (c, _, err) = ref_cmd(args).await;
            assert_eq!(c, code(2), "{args:?}");
            assert!(err.contains("--profile"), "{err}");
        }
    }

    #[tokio::test]
    async fn a_failed_command_prints_its_error_and_exits_with_code_2() {
        let (c, _, err) = ref_cmd(&["pdf", "/no/such/file.pdf"]).await;
        // A missing file is that file's failure, not the command's
        assert_eq!(c, code(1));
        let _ = err;
        let (c, _, err) = ref_cmd(&["fetch", "--user-agent", "  ", "https://example.com"]).await;
        assert_eq!(c, code(2));
        assert!(err.contains("user agent is empty"), "{err}");
    }

    #[tokio::test]
    async fn a_command_that_succeeds_exits_with_code_0_and_prints_json() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("references.yaml");
        let (c, out, _) = ref_cmd(&["init", "--output", file.to_str().unwrap()]).await;
        assert_eq!(c, ExitCode::SUCCESS);
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(json["created"].is_string(), "{out}");
        // Again without --force: the command refuses
        let (c, _, err) = ref_cmd(&["init", "--output", file.to_str().unwrap()]).await;
        assert_eq!(c, code(2));
        assert!(err.contains("File exists"), "{err}");
    }

    #[tokio::test]
    async fn the_stop_signal_ends_the_command_with_code_130() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let args = ["ref", "check-links", "--url", "http://127.0.0.1:9/"];
        let code = execute(args, &mut out, &mut err, std::future::ready(())).await;
        assert_eq!(code, ExitCode::from(130));
        assert_eq!(String::from_utf8_lossy(&err), "Interrupted.\n");
    }

    #[tokio::test]
    async fn the_mcp_command_refuses_a_profile_it_cannot_use() {
        let (c, _, err) = ref_cmd(&["mcp", "--root", "/no/such/folder"]).await;
        assert_eq!(c, code(2));
        assert!(err.contains("Workspace folder"), "{err}");
    }

    #[tokio::test]
    async fn a_login_with_fresh_is_refused_before_chrome_starts() {
        let (c, _, err) = ref_cmd(&["login", "--profile", "fresh"]).await;
        assert_eq!(c, code(2));
        assert!(err.contains("needs a folder"), "{err}");
    }

    #[test]
    fn the_program_says_so_when_the_runtime_cannot_start() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let failed = Err(std::io::Error::other("no threads"));
        let code = start(failed, ["ref"], &mut out, &mut err);
        assert_eq!(code, ExitCode::from(2));
        let text = String::from_utf8_lossy(&err);
        assert!(
            text.contains("cannot start the async runtime: no threads"),
            "{text}"
        );
    }

    #[test]
    fn the_program_runs_a_command_on_its_own_runtime() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let runtime = tokio::runtime::Runtime::new();
        let code = start(runtime, ["ref", "--version"], &mut out, &mut err);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(String::from_utf8_lossy(&out).contains("ref"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_is_a_stop_signal() {
        let waiting = tokio::spawn(shutdown_signal());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let sent = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let done = tokio::time::timeout(std::time::Duration::from_secs(5), waiting).await;
        assert!(done.is_ok(), "SIGTERM did not end the wait");
    }
}
