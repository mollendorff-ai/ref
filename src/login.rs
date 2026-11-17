//! login command: open a visible Chrome with ref's saved profile so you can
//! sign in to your sites once.
//!
//! Everything you do in that window (signing in, accepting cookies, passing
//! "I am human" checks) is kept in the saved profile, and later `ref fetch`,
//! `ref pdf` and MCP runs reuse those sessions in headless Chrome.

use crate::browser::{detect_chrome_path, prepare_saved_profile, running_as_root, want_sandbox};
use crate::profile::{self, ProfileChoice};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Args)]
pub struct LoginArgs {
    /// Pages to open, for example the sites you want to sign in to
    pub urls: Vec<String>,

    /// Profile folder to sign in with [default: `REF_CHROME_PROFILE` when set,
    /// else ~/.ref/chrome-profile]
    #[arg(long, value_name = "FOLDER", value_parser = profile::parse_profile)]
    pub profile: Option<ProfileChoice>,
}

/// Switches shared with the headless runs, so cookies saved here can be read
/// there (both encrypt them the same way).
const LOGIN_SWITCHES: [&str; 4] = [
    "--no-first-run",
    "--no-default-browser-check",
    "--password-store=basic",
    "--use-mock-keychain",
];

/// The folder to sign in with: the `--profile` folder, else the default saved profile.
///
/// # Errors
/// Refuses `fresh`, whose sign-ins would be deleted with it, and fails
/// without a home folder when no folder is given.
fn login_dir(choice: Option<&ProfileChoice>, home: Option<&Path>) -> Result<PathBuf> {
    match choice {
        Some(ProfileChoice::Folder(dir)) => Ok(dir.clone()),
        Some(ProfileChoice::Fresh) => bail!(
            "ref login needs a folder: a `fresh` profile is deleted afterwards, so the \
             sign-ins would be lost. Leave out --profile (and REF_CHROME_PROFILE) to use \
             ~/.ref/chrome-profile, or give a folder."
        ),
        None => profile::default_saved_dir(home)
            .context("No home folder found; pass --profile <folder>"),
    }
}

/// Run the login command: blocks until that Chrome is closed.
///
/// # Errors
/// Returns an error when no profile folder can be chosen, the profile is in
/// use or is Chrome's everyday profile, a URL is not http(s), or Chrome cannot start.
pub fn run_login(args: &LoginArgs, out: &mut impl std::io::Write) -> Result<()> {
    let home = profile::home_dir();
    let dir = login_dir(
        profile::resolve(args.profile.as_ref())?.as_ref(),
        home.as_deref(),
    )?;

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
    writeln!(out, "{output}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_signs_in_to_a_folder_never_a_fresh_profile() {
        let folder = PathBuf::from("/work/p");
        let given = login_dir(Some(&ProfileChoice::Folder(folder.clone())), None).unwrap();
        assert_eq!(given, folder);
        let default = login_dir(None, Some(Path::new("/home/me"))).unwrap();
        assert_eq!(default, PathBuf::from("/home/me/.ref/chrome-profile"));
        let fresh = login_dir(Some(&ProfileChoice::Fresh), None).unwrap_err();
        assert!(fresh.to_string().contains("needs a folder"), "{fresh}");
        let homeless = login_dir(None, None).unwrap_err();
        assert!(
            homeless.to_string().contains("No home folder"),
            "{homeless}"
        );
    }
}
