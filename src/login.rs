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
use std::ffi::OsString;
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

/// The folder to sign in with: the `--profile` folder, else the folder in
/// `REF_CHROME_PROFILE`, else the default saved profile. `fresh` in the
/// variable is ignored (it is meant for the commands that fetch).
///
/// # Errors
/// Refuses `--profile fresh`, whose sign-ins would be deleted with it, and
/// fails without a home folder when no folder is given.
fn login_dir(
    flag: Option<&ProfileChoice>,
    from_env: Option<&ProfileChoice>,
    home: Option<&Path>,
) -> Result<PathBuf> {
    match (flag, from_env) {
        (Some(ProfileChoice::Fresh), _) => bail!(
            "ref login needs a folder: a `fresh` profile is deleted afterwards, so the \
             sign-ins would be lost. Leave out --profile to use ~/.ref/chrome-profile, \
             or give a folder."
        ),
        (Some(ProfileChoice::Folder(dir)), _) | (None, Some(ProfileChoice::Folder(dir))) => {
            Ok(dir.clone())
        }
        (None, _) => profile::default_saved_dir(home)
            .context("No home folder found; pass --profile <folder>"),
    }
}

/// Chrome's command line for signing in to `dir`, opening `urls`.
fn login_args(dir: &Path, urls: &[String], sandbox: bool) -> Vec<OsString> {
    let mut args = vec![OsString::from(format!("--user-data-dir={}", dir.display()))];
    args.extend(LOGIN_SWITCHES.map(OsString::from));
    args.extend(urls.iter().map(OsString::from));
    if !sandbox {
        args.push("--no-sandbox".into());
    }
    args
}

/// Run the login command: blocks until that Chrome is closed.
///
/// # Errors
/// Returns an error when no profile folder can be chosen, the profile is in
/// use or is Chrome's everyday profile, a URL is not http(s), or Chrome cannot start.
pub fn run_login(args: &LoginArgs, out: &mut dyn std::io::Write) -> Result<()> {
    run_login_with(args, detect_chrome_path(), out)
}

/// [`run_login`] with the Chrome to start (`None` when none was found)
fn run_login_with(
    args: &LoginArgs,
    chrome: Option<PathBuf>,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let home = profile::home_dir();
    let from_env = profile::resolve(None)?;
    let dir = login_dir(args.profile.as_ref(), from_env.as_ref(), home.as_deref())?;

    for url in &args.urls {
        if let Err(e) = crate::guard::check_web_url(url) {
            bail!("{url}: {e}");
        }
    }
    prepare_saved_profile(&dir, home.as_deref())?;
    let chrome = chrome.context("Chrome/Chromium not found")?;
    let sandbox = want_sandbox(
        std::env::var(crate::browser::NO_SANDBOX_ENV)
            .ok()
            .as_deref(),
        running_as_root(),
    );

    eprintln!("Chrome is opening with ref's saved profile:");
    eprintln!("  {}", dir.display());
    eprintln!("Sign in to your sites and pass any checks, then quit Chrome completely.");
    eprintln!("ref will reuse these sessions from now on.");
    let status = Command::new(chrome)
        .args(login_args(&dir, &args.urls, sandbox))
        .status()
        .context("Failed to start Chrome")?;

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
        let folder = ProfileChoice::Folder(PathBuf::from("/work/p"));
        let other = ProfileChoice::Folder(PathBuf::from("/from/env"));
        let home = Some(Path::new("/home/me"));
        let default = PathBuf::from("/home/me/.ref/chrome-profile");

        // The option wins over the variable; the variable wins over the default
        let given = login_dir(Some(&folder), Some(&other), home).unwrap();
        assert_eq!(given, PathBuf::from("/work/p"));
        let from_env = login_dir(None, Some(&other), home).unwrap();
        assert_eq!(from_env, PathBuf::from("/from/env"));
        assert_eq!(login_dir(None, None, home).unwrap(), default);

        // `fresh` in the variable is for the fetching commands: login ignores it
        let ignored = login_dir(None, Some(&ProfileChoice::Fresh), home).unwrap();
        assert_eq!(ignored, default);
        // Asked for by name, it is refused
        let fresh = login_dir(Some(&ProfileChoice::Fresh), Some(&other), home).unwrap_err();
        assert!(fresh.to_string().contains("needs a folder"), "{fresh}");

        let homeless = login_dir(None, None, None).unwrap_err();
        assert!(
            homeless.to_string().contains("No home folder"),
            "{homeless}"
        );
    }

    #[test]
    fn chrome_opens_the_folder_and_the_pages() {
        let urls = vec!["https://a.example/".to_string()];
        let sandboxed = login_args(Path::new("/p"), &urls, true);
        let expected = [
            "--user-data-dir=/p",
            "--no-first-run",
            "--no-default-browser-check",
            "--password-store=basic",
            "--use-mock-keychain",
            "https://a.example/",
        ];
        assert_eq!(sandboxed, expected);
        let unsandboxed = login_args(Path::new("/p"), &urls, false);
        assert_eq!(unsandboxed[..6], expected);
        assert_eq!(unsandboxed[6], "--no-sandbox");
    }

    /// Sign in to a new folder with `chrome` standing in for Chrome
    fn login_with(urls: &[&str], chrome: Option<&str>) -> (tempfile::TempDir, Result<String>) {
        let dir = tempfile::tempdir().unwrap();
        let args = LoginArgs {
            urls: urls.iter().map(ToString::to_string).collect(),
            profile: Some(ProfileChoice::Folder(dir.path().join("profile"))),
        };
        let mut out = Vec::new();
        let done = run_login_with(&args, chrome.map(PathBuf::from), &mut out);
        (dir, done.map(|()| String::from_utf8(out).unwrap()))
    }

    #[test]
    fn closing_chrome_saves_the_profile() {
        let (dir, out) = login_with(&["https://a.example/"], Some("/usr/bin/true"));
        let out: serde_json::Value = serde_json::from_str(&out.unwrap()).unwrap();
        let profile = dir.path().join("profile");
        assert_eq!(out["profile"], profile.display().to_string());
        assert_eq!(out["status"], "saved");
        assert!(profile.is_dir());
    }

    #[test]
    fn a_chrome_that_fails_is_reported() {
        let (_dir, out) = login_with(&[], Some("/usr/bin/false"));
        let out: serde_json::Value = serde_json::from_str(&out.unwrap()).unwrap();
        assert_eq!(out["status"], "chrome_exited_with_error");
    }

    #[test]
    fn login_needs_a_chrome_it_can_start() {
        let (_dir, missing) = login_with(&[], None);
        assert_eq!(
            missing.unwrap_err().to_string(),
            "Chrome/Chromium not found"
        );
        let (_dir, broken) = login_with(&[], Some("/nonexistent/chrome"));
        assert_eq!(broken.unwrap_err().to_string(), "Failed to start Chrome");
    }

    #[test]
    fn a_page_that_is_not_on_the_web_is_refused_before_anything_starts() {
        let (dir, out) = login_with(&["file:///etc/passwd"], Some("/usr/bin/true"));
        let err = out.unwrap_err().to_string();
        assert!(err.starts_with("file:///etc/passwd: "), "{err}");
        assert!(!dir.path().join("profile").exists());
    }
}
