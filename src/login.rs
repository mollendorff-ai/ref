//! login command: open a visible Chrome with ref's saved profile so you can
//! sign in to your sites once.
//!
//! Everything you do in that window (signing in, accepting cookies, passing
//! "I am human" checks) is kept in the saved profile, and later `ref fetch`,
//! `ref pdf` and MCP runs reuse those sessions in headless Chrome.

use crate::browser::{detect_chrome_path, prepare_saved_profile, running_as_root, want_sandbox};
use crate::profile;
use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::PathBuf;
use std::process::Command;

#[derive(Args)]
pub struct LoginArgs {
    /// Pages to open, for example the sites you want to sign in to
    pub urls: Vec<String>,

    /// Profile folder to sign in with [default: ~/.ref/chrome-profile,
    /// or `REF_CHROME_PROFILE` when it names a folder]
    #[arg(long)]
    pub profile: Option<PathBuf>,
}

/// Switches shared with the headless runs, so cookies saved here can be read
/// there (both encrypt them the same way).
const LOGIN_SWITCHES: [&str; 4] = [
    "--no-first-run",
    "--no-default-browser-check",
    "--password-store=basic",
    "--use-mock-keychain",
];

/// Run the login command: blocks until that Chrome is closed.
///
/// # Errors
/// Returns an error when no profile folder can be chosen, the profile is in
/// use or is Chrome's everyday profile, a URL is not http(s), or Chrome cannot start.
pub fn run_login(args: &LoginArgs) -> Result<()> {
    let home = profile::home_dir();
    let from_env = std::env::var(profile::PROFILE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty() && !v.trim().eq_ignore_ascii_case("fresh"))
        .map(PathBuf::from);
    let dir = args
        .profile
        .clone()
        .or(from_env)
        .or_else(|| profile::default_saved_dir(home.as_deref()))
        .context("No home folder found; pass --profile <folder>")?;

    for url in &args.urls {
        if let Err(e) = crate::guard::check_web_url(url) {
            bail!("{url}: {e}");
        }
    }
    prepare_saved_profile(&dir, home.as_deref())?;
    let chrome = detect_chrome_path().context("Chrome/Chromium not found")?;

    let mut cmd = Command::new(chrome);
    cmd.arg(format!("--user-data-dir={}", dir.display()))
        .args(LOGIN_SWITCHES)
        .args(&args.urls);
    if !want_sandbox(
        std::env::var(crate::browser::NO_SANDBOX_ENV)
            .ok()
            .as_deref(),
        running_as_root(),
    ) {
        cmd.arg("--no-sandbox");
    }

    eprintln!("Chrome is opening with ref's saved profile:");
    eprintln!("  {}", dir.display());
    eprintln!("Sign in to your sites and pass any checks, then quit Chrome completely.");
    eprintln!("ref will reuse these sessions from now on.");
    let status = cmd.status().context("Failed to start Chrome")?;

    let output = serde_json::json!({
        "profile": dir.display().to_string(),
        "status": if status.success() { "saved" } else { "chrome_exited_with_error" },
    });
    println!("{output}");
    Ok(())
}
