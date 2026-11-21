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
use std::io::{BufRead, Write};
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

/// A command ended early by the stop signal (Ctrl-C or SIGTERM): exit code 130
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Interrupted.")
    }
}

impl std::error::Error for Interrupted {}

/// The stop signal as [`execute`] receives it, shared with `ref mcp`
type Stop = futures::future::Shared<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>;

/// Run the chosen command, writing its JSON to `out`: `true` when everything
/// succeeded, `false` when some URLs or files failed. `ref mcp` watches `stop`
/// itself, so it can stop Chrome gracefully first; other commands are dropped
/// by [`execute`] when it finishes.
///
/// # Errors
/// Returns the command's error (bad arguments, Chrome missing, I/O error), or
/// [`Interrupted`] when `ref mcp` was stopped.
pub async fn run(
    cli: Cli,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    stop: Stop,
) -> Result<bool> {
    match cli.command {
        Commands::Fetch(args) => run_fetch(args, out).await,
        Commands::Pdf(args) => run_pdf(&args, out).await,
        Commands::Init(args) => run_init(&args, out).map(|()| true),
        Commands::Login(args) => run_login(&args, out).map(|()| true),
        Commands::Scan(args) => run_scan(args, out).await.map(|()| true),
        Commands::CheckLinks(args) => run_check_links(args, input, out).await,
        Commands::RefreshData(args) => run_refresh_data(args, out).await,
        Commands::VerifyRefs(args) => run_verify_refs(args, out).await,
        Commands::Update(args) => run_update(args, out).await.map(|()| true),
        Commands::Mcp(args) => {
            let profile = args.profile.choice()?;
            let stop = Box::pin(stop);
            crate::mcp::run_mcp(args.root, profile, stop)
                .await
                .map(|()| true)
        }
    }
}

/// Exit codes: 0 = everything succeeded, 1 = some URLs or files failed,
/// 2 = the command itself failed (bad arguments, Chrome missing, I/O error),
/// 130 = interrupted.
///
/// `args` are the command line (first the program name); `input` is the
/// standard input (`check-links --stdin` reads URLs from it); the command's JSON
/// goes to `out` and its error to `err`. When `stop` finishes first (Ctrl-C or
/// SIGTERM in the real program), the command is dropped, which closes Chrome
/// and removes its temporary profile, instead of leaving them behind.
pub async fn execute(
    args: Vec<std::ffi::OsString>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
    stop: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
) -> ExitCode {
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
    let stop = futures::FutureExt::shared(stop);
    let stops_itself = matches!(cli.command, Commands::Mcp(_));
    let result = tokio::select! {
        r = run(cli, input, out, stop.clone()) => r,
        () = stop, if !stops_itself => Err(anyhow::Error::new(Interrupted)),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) if e.is::<Interrupted>() => {
            let _ = writeln!(err, "{e}");
            ExitCode::from(130)
        }
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
    real(std::env::args_os().collect())
}

/// [`main`] with the command line given, and everything else real: standard
/// input, output and error, the stop signals and the async runtime. The
/// process-level tests run it in a child process.
fn real(args: Vec<std::ffi::OsString>) -> ExitCode {
    start(
        tokio::runtime::Runtime::new(),
        args,
        &mut real_input(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

/// Standard input for `check-links --stdin`, locked only while each read runs.
/// Holding the lock for the whole run would block every other reader of
/// standard input for good: `ref mcp` reads its requests through tokio, which
/// takes the same lock on another thread.
fn real_input() -> std::io::BufReader<std::io::Stdin> {
    std::io::BufReader::new(std::io::stdin())
}

/// [`main`] with its parts given, so the runtime failing to start can be tested
fn start(
    runtime: std::io::Result<tokio::runtime::Runtime>,
    args: Vec<std::ffi::OsString>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let runtime = match runtime {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(err, "Error: cannot start the async runtime: {e}");
            return ExitCode::from(2);
        }
    };
    let code = runtime.block_on(execute(args, input, out, err, Box::pin(shutdown_signal())));
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
        // Without a SIGTERM handler, that branch is off and Ctrl-C still works
        let mut term = signal(SignalKind::terminate()).ok();
        let terminated = futures::future::OptionFuture::from(
            term.as_mut().map(tokio::signal::unix::Signal::recv),
        );
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            Some(_) = terminated => {}
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

    use crate::test_server::ref_cmd;

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
        let args = ["ref", "check-links", "--url", "http://127.0.0.1:9/"]
            .map(Into::into)
            .to_vec();
        let code = execute(
            args,
            &mut &b""[..],
            &mut out,
            &mut err,
            Box::pin(std::future::ready(())),
        )
        .await;
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
    async fn verify_refs_without_its_file_is_a_command_error() {
        let (c, _, err) = ref_cmd(&["verify-refs", "/no/such/references.yaml"]).await;
        assert_eq!(c, code(2));
        assert!(err.contains("Failed to read"), "{err}");
    }

    #[tokio::test]
    async fn update_is_dispatched() {
        // Only that the command runs: it asks GitHub, so it answers (0) or,
        // offline, fails as a command (2). Real tests of update need a fake
        // GitHub, which comes with the update module's own tests.
        let (c, _, _) = ref_cmd(&["update", "--check"]).await;
        assert!(c == ExitCode::SUCCESS || c == code(2), "{c:?}");
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
        let code = start(
            failed,
            vec!["ref".into()],
            &mut &b""[..],
            &mut out,
            &mut err,
        );
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
        let code = start(
            runtime,
            vec!["ref".into(), "--version".into()],
            &mut &b""[..],
            &mut out,
            &mut err,
        );
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(String::from_utf8_lossy(&out).contains("ref"));
    }

    #[test]
    fn the_real_program_reads_its_own_command_line() {
        // The test program's arguments are not a ref command line: ref says so
        // and exits with 2 (or shows help, 0, if run with a filter named help)
        let code = format!("{:?}", main());
        let refused = [ExitCode::from(2), ExitCode::SUCCESS].map(|c| format!("{c:?}"));
        assert!(refused.contains(&code), "{code}");
    }

    // Process-level tests (ADR-006): the test program starts itself again as a
    // child, which runs `real` with real standard input, output, error and
    // signals. The child is chosen by full test path with --exact; a short name
    // would match no test, so every parent test also checks the child's output.

    const CHILD_ARGS: &str = "REF_TEST_CHILD_ARGS";

    /// The child half: run `real` with the command line in `CHILD_ARGS` (joined
    /// by newlines) and end the process with its exit code. A normal test run
    /// has no such variable, and this does nothing.
    #[test]
    fn child_runs_the_real_program() {
        let Ok(line) = std::env::var(CHILD_ARGS) else {
            return;
        };
        let args = line.split('\n').map(Into::into).collect();
        let code = real(args);
        let n = (0..=u8::MAX).find(|n| ExitCode::from(*n) == code);
        std::process::exit(i32::from(n.unwrap_or(u8::MAX)));
    }

    /// The child process; killed when the test ends, however it ends
    struct RefProcess(std::process::Child);

    impl Drop for RefProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
        }
    }

    /// Start the child as `ref <args>` with piped standard input, output and error
    fn spawn_ref(args: &[&str]) -> RefProcess {
        spawn_ref_in(args, &[])
    }

    /// [`spawn_ref`] with environment variables set in the child; the two ref
    /// reads for the profile are cleared first, so the child sees only these
    fn spawn_ref_in(args: &[&str], env: &[(&str, &std::path::Path)]) -> RefProcess {
        let line = std::iter::once("ref").chain(args.iter().copied());
        RefProcess(
            std::process::Command::new(std::env::current_exe().expect("test program"))
                .env_remove(crate::profile::PROFILE_ENV)
                .envs(env.iter().copied())
                .args([
                    "cli::tests::child_runs_the_real_program",
                    "--exact",
                    "--nocapture",
                ])
                .env(CHILD_ARGS, line.collect::<Vec<_>>().join("\n"))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("start the child"),
        )
    }

    /// The child's output lines, read on a thread so a child that says nothing
    /// fails its test after a time limit instead of hanging it
    fn output_lines(out: std::process::ChildStdout) -> std::sync::mpsc::Receiver<String> {
        let (sent, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                let _ = sent.send(line);
            }
        });
        lines
    }

    /// Wait for the JSON-RPC reply with `id` (the harness's own lines are skipped)
    fn reply_with_id(lines: &std::sync::mpsc::Receiver<String>, id: u64) -> serde_json::Value {
        loop {
            let line = lines
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("a reply within 30 seconds");
            let value: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            if value["id"] == id {
                return value;
            }
        }
    }

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;

    #[test]
    fn the_saved_profile_is_found_in_the_real_home_folder() {
        // No --profile and no REF_CHROME_PROFILE: ref looks for
        // <home>/.ref/chrome-profile, with the home folder from HOME
        let page = crate::test_server::serve();
        let home = tempfile::tempdir().unwrap();
        let saved = home.path().join(".ref").join("chrome-profile");
        std::fs::create_dir_all(&saved).unwrap();
        let mut child = spawn_ref_in(
            &["fetch", &format!("{page}/report")],
            &[("HOME", home.path())],
        );
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.0.stderr.take().unwrap(), &mut stderr).unwrap();
        assert!(child.0.wait().unwrap().success(), "{stderr}");
        assert!(
            stderr.contains(&format!("Using saved profile {}", saved.display())),
            "{stderr}"
        );
    }

    #[test]
    fn mcp_answers_requests_on_real_standard_input_and_exits_0() {
        let page = crate::test_server::serve();
        let mut child = spawn_ref(&["mcp", "--profile", "fresh"]);
        let mut input = child.0.stdin.take().expect("stdin");
        let out = output_lines(child.0.stdout.take().expect("stdout"));
        let call = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"ref_fetch","arguments":{"urls":[format!("{page}/")]}}});
        writeln!(input, "{INITIALIZE}").expect("send");
        let init = reply_with_id(&out, 1);
        assert_eq!(init["result"]["serverInfo"]["name"], "ref");
        writeln!(
            input,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .expect("send");
        writeln!(input, "{call}").expect("send");
        let fetched = reply_with_id(&out, 2);
        let text = fetched["result"]["content"][0]["text"]
            .as_str()
            .expect("text");
        assert!(text.contains(&page), "{text}");
        drop(input);
        assert_eq!(child.0.wait().expect("wait").code(), Some(0));
    }

    #[test]
    fn check_links_reads_urls_from_real_standard_input() {
        let page = crate::test_server::serve();
        let mut child = spawn_ref(&["check-links", "--profile", "fresh", "--stdin"]);
        let mut input = child.0.stdin.take().expect("stdin");
        let out = output_lines(child.0.stdout.take().expect("stdout"));
        writeln!(input, "{page}/").expect("send");
        drop(input);
        // The test harness prints its own "running 1 test" line first
        let report = loop {
            let line = out
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("a report within 30 seconds");
            if line.starts_with('{') {
                break serde_json::from_str::<serde_json::Value>(&line).expect("JSON");
            }
        };
        assert_eq!(report["results"][0]["url"], format!("{page}/"));
        assert!(child.0.wait().expect("wait").code().is_some_and(|c| c <= 1));
    }

    #[cfg(unix)]
    #[test]
    fn sigterm_stops_a_running_mcp_server_with_exit_code_130() {
        let mut child = spawn_ref(&["mcp", "--profile", "fresh"]);
        let mut input = child.0.stdin.take().expect("stdin");
        let out = output_lines(child.0.stdout.take().expect("stdout"));
        writeln!(input, "{INITIALIZE}").expect("send");
        reply_with_id(&out, 1);
        let sent = std::process::Command::new("kill")
            .args(["-TERM", &child.0.id().to_string()])
            .status()
            .expect("kill");
        assert!(sent.success());
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.0.stderr.take().expect("stderr"), &mut stderr)
            .expect("read");
        assert_eq!(child.0.wait().expect("wait").code(), Some(130));
        assert!(stderr.contains("Interrupted."), "{stderr}");
    }

    #[test]
    fn the_real_input_leaves_standard_input_unlocked() {
        // ref mcp reads standard input on another thread while main holds its
        // input; that thread must be able to take the lock
        let _input = real_input();
        let (sent, got) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(std::io::stdin().lock());
            let _ = sent.send(());
        });
        assert!(got.recv_timeout(std::time::Duration::from_secs(5)).is_ok());
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
