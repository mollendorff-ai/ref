//! Starting Chrome, its switches and user agent, and the processes and
//! profile folders it leaves behind

use crate::cookies::Cookie;
use crate::profile;
use anyhow::{Context, Result};
use chromiumoxide::browser::BrowserConfigBuilder;
use chromiumoxide::cdp::browser_protocol::network::{CookieParam, TimeSinceEpoch};
use chromiumoxide::{Browser, BrowserConfig};
use std::path::{Path, PathBuf};

/// Auto-detect Chrome/Chromium executable path based on OS
pub(crate) fn detect_chrome_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let candidates = [
        "/usr/bin/google-chrome-stable",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium-browser",
        "/usr/bin/chromium",
        "/snap/bin/chromium",
        "/usr/local/bin/chrome",
        "/usr/local/bin/chromium",
    ];

    #[cfg(target_os = "macos")]
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    ];

    #[cfg(target_os = "windows")]
    let candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        &format!(
            r"{}\Google\Chrome\Application\chrome.exe",
            std::env::var("LOCALAPPDATA").unwrap_or_default()
        ),
    ];

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let candidates: [&str; 0] = [];

    first_existing(&candidates)
}

/// The first of `paths` that exists
fn first_existing(paths: &[&str]) -> Option<PathBuf> {
    paths.iter().map(PathBuf::from).find(|p| p.exists())
}

/// Fresh profile folders not removed yet, each with a token its pool holds,
/// so the final clean-up can tell a pool that is gone from one still in use
/// (see [`remove_open_profiles`]).
type Tracked = Vec<(PathBuf, std::sync::Weak<()>)>;
static OPEN_PROFILES: std::sync::Mutex<Tracked> = std::sync::Mutex::new(Vec::new());

fn tracked() -> std::sync::MutexGuard<'static, Tracked> {
    OPEN_PROFILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Track `dir` for the final clean-up while `owner` (held by its pool) lives
pub(super) fn track_profile(dir: &Path, owner: &std::sync::Arc<()>) {
    let mut dirs = tracked();
    dirs.retain(|(d, _)| d != dir);
    dirs.push((dir.to_path_buf(), std::sync::Arc::downgrade(owner)));
}

/// Stop tracking `dir` once it is gone; while it is still on disk it stays
/// on the list for the final clean-up
pub(super) fn untrack_profile(dir: &Path) {
    tracked().retain(|(d, _)| d != dir || d.exists());
}

/// Whether `dir` is on the list for the final clean-up
#[cfg(test)]
pub(super) fn is_tracked(dir: &Path) -> bool {
    tracked().iter().any(|(d, _)| d == dir)
}

/// Remove the fresh profiles of pools that are gone without removing them:
/// when ref was interrupted (Ctrl-C or SIGTERM), their Chrome was killed.
/// Profiles of pools still in use are left alone, so this is safe to call
/// while other work runs (tests run the program in-process).
pub fn remove_open_profiles() {
    let mut dirs = tracked();
    for (dir, owner) in dirs.iter() {
        if owner.upgrade().is_none() {
            remove_dir_retrying(dir);
        }
    }
    dirs.retain(|(dir, owner)| owner.upgrade().is_some() || dir.exists());
}

/// Remove a profile folder. Chrome's helper processes can still write to it for
/// a moment after a kill or a crash while they exit, so remove, and while the
/// folder is still there wait and remove again (at most 3 times, 300 ms apart).
/// A folder that is gone costs no waiting.
pub(super) fn remove_dir_retrying(dir: &Path) {
    for attempt in 0..3 {
        let _ = std::fs::remove_dir_all(dir);
        if !dir.exists() {
            return;
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
}

/// Extra Chrome switches as (name, value) pairs, without the leading `--`
/// (chromiumoxide adds it; an empty value means a bare switch).
///
/// Most keep Chrome from calling Google (component updates, sync, sign-in,
/// safe browsing, metrics, field trials) while it renders pages for ref.
const CHROME_SWITCHES: [(&str, &str); 21] = [
    ("headless", "new"),
    ("disable-gpu", ""),
    ("disable-dev-shm-usage", ""),
    ("no-first-run", ""),
    ("no-default-browser-check", ""),
    ("disable-background-networking", ""),
    ("disable-component-update", ""),
    ("disable-sync", ""),
    ("disable-default-apps", ""),
    ("disable-extensions", ""),
    ("disable-component-extensions-with-background-pages", ""),
    ("disable-domain-reliability", ""),
    ("disable-client-side-phishing-detection", ""),
    ("disable-breakpad", ""),
    ("disable-field-trial-config", ""),
    ("metrics-recording-only", ""),
    ("no-pings", ""),
    ("no-service-autorun", ""),
    ("password-store", "basic"),
    ("use-mock-keychain", ""),
    ("safebrowsing-disable-auto-update", ""),
];

/// Chrome features to turn off. Passed under the same `disable-features` key
/// chromiumoxide uses (its default is `TranslateUI`), so its builder merges
/// both lists into one switch
/// (Chrome keeps only the last of repeated switches, and the order of
/// separately keyed switches is random).
const DISABLED_FEATURES: [&str; 16] = [
    "Translate",
    "AutofillServerCommunication",
    "NetworkTimeServiceQuerying",
    "PreconnectToSearch",
    "OptimizationHints",
    "OptimizationGuideModelDownloading",
    "MediaRouter",
    "DialMediaRouteProvider",
    "CertificateTransparencyComponentUpdater",
    "InterestFeedContentSuggestions",
    "AccountConsistency",
    "GlobalMediaControls",
    "SafeBrowsingEnhancedProtection",
    "PrivacySandboxSettings4",
    "AimEnabled",
    "WebUIOmniboxPopup",
];

/// Apply [`CHROME_SWITCHES`] and [`DISABLED_FEATURES`] to a launch config.
fn with_chrome_args(mut builder: BrowserConfigBuilder) -> BrowserConfigBuilder {
    for (key, value) in CHROME_SWITCHES {
        builder = if value.is_empty() {
            builder.arg(key)
        } else {
            builder.arg((key, value))
        };
    }
    builder.arg(("disable-features", &DISABLED_FEATURES[..]))
}

pub(super) async fn launch(
    chrome_path: &Path,
    profile_dir: &Path,
    sandbox: bool,
) -> Result<(Browser, chromiumoxide::Handler)> {
    // chromiumoxide adds the leading "--" itself, so flags are written without it
    // (written with it, Chrome receives "----flag" and silently ignores it).
    let mut builder = BrowserConfig::builder()
        .chrome_executable(chrome_path)
        .user_data_dir(profile_dir);
    builder = with_chrome_args(builder);
    if !sandbox {
        builder = builder.no_sandbox();
    }
    let config = builder
        .build()
        .map_err(anyhow::Error::msg)
        .context("Browser config error")?;

    Browser::launch(config)
        .await
        .context("Failed to launch Chrome")
}

/// Make sure a saved profile can be used: not Chrome's everyday folder (Chrome
/// 136+ refuses remote control there), created if missing, and not open in
/// another Chrome.
///
/// # Errors
/// Returns an error explaining what to do instead.
pub(crate) fn prepare_saved_profile(dir: &Path, home: Option<&Path>) -> Result<()> {
    if profile::is_chrome_default_dir(dir, home) {
        anyhow::bail!(
            "{} is your everyday Chrome profile. Chrome (version 136 and later) does not \
             let tools like ref control that profile, to protect its cookies. Use ref's \
             own saved profile instead: run `ref login` (without --profile), sign in to \
             your sites there once, and ref reuses those sessions.",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Cannot create the profile folder {}", dir.display()))?;
    if let Some(pid) = profile_in_use(dir) {
        anyhow::bail!(
            "The profile {} is open in another Chrome (process {pid}): a `ref login` window, \
             a running `ref mcp` server or another ref command. Quit it fully (for `ref login`, \
             close every window) and try again.",
            dir.display()
        );
    }
    Ok(())
}

/// The live process holding Chrome's `SingletonLock` in `dir`, if any
fn profile_in_use(dir: &Path) -> Option<u32> {
    let target = std::fs::read_link(dir.join("SingletonLock")).ok()?;
    let pid = profile::lock_owner_pid(&target.to_string_lossy())?;
    process_alive(pid).then_some(pid)
}

#[cfg(target_os = "linux")]
pub(super) fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Kill process `pid` outright
#[cfg(unix)]
pub(super) fn kill_process(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

#[cfg(not(unix))]
pub(super) fn kill_process(_pid: u32) {}

#[cfg(all(unix, not(target_os = "linux")))]
pub(super) fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(unix))]
pub(super) fn process_alive(_pid: u32) -> bool {
    false
}

/// Clean up after ref runs that were killed outright (SIGKILL, power loss):
/// their Chrome may still be running and their profile is still on disk.
/// A profile is stale when the ref process named in it no longer exists.
#[cfg(target_os = "linux")]
pub(super) fn sweep_orphans() {
    sweep_orphans_in(&std::env::temp_dir());
}

/// [`sweep_orphans`] for the profile folders in `folder`
#[cfg(target_os = "linux")]
fn sweep_orphans_in(folder: &Path) {
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = profile_owner(&name) else {
            continue;
        };
        if pid == std::process::id() || Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let dir = entry.path();
        kill_chrome_using(&dir);
        remove_dir_retrying(&dir);
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn sweep_orphans() {}

/// Kill every process whose command line uses `profile` as its user-data-dir
#[cfg(target_os = "linux")]
fn kill_chrome_using(profile: &Path) {
    let needle = format!("--user-data-dir={}", profile.display());
    for proc_entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = proc_entry.file_name().to_string_lossy().parse() else {
            continue;
        };
        let cmdline = std::fs::read(proc_entry.path().join("cmdline")).unwrap_or_default();
        if cmdline
            .split(|b| *b == 0)
            .any(|arg| arg == needle.as_bytes())
        {
            kill_process(pid);
        }
    }
}

/// The ref process id in a profile folder name `ref-chrome-<pid>-<nanos>`
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only the Linux sweep uses it
fn profile_owner(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("ref-chrome-")?;
    let (pid, nanos) = rest.split_once('-')?;
    nanos.parse::<u32>().ok()?;
    pid.parse().ok()
}

/// A profile folder unique to this process and pool
pub(super) fn unique_profile_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    std::env::temp_dir().join(format!("ref-chrome-{}-{nanos}", std::process::id()))
}

/// Chrome refuses its sandbox as root; users can also opt out explicitly.
pub(crate) fn want_sandbox(env_value: Option<&str>, root: bool) -> bool {
    let opted_out = env_value.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    !opted_out && !root
}

#[cfg(target_os = "linux")]
pub(crate) fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|s| uid_is_root(&s))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn running_as_root() -> bool {
    false
}

/// Parse the `Uid:` line of `/proc/self/status` (real, effective, ...).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn uid_is_root(status: &str) -> bool {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().nth(1))
        == Some("0")
}

/// Check a user agent given by the user: not empty, one line, no control characters
/// (it goes into a request header). Surrounding spaces are trimmed.
///
/// # Errors
/// Returns an error saying what is wrong with it.
pub fn validate_user_agent(user_agent: &str) -> Result<String> {
    let ua = user_agent.trim();
    if ua.is_empty() {
        anyhow::bail!("The user agent is empty. Give a name and an email, e.g. \"Jane Doe jane@example.com\".");
    }
    if ua.chars().any(char::is_control) {
        anyhow::bail!("The user agent must be one line without control characters.");
    }
    Ok(ua.to_string())
}

/// Chrome's own user agent, without the "Headless" marker hidden and with
/// ref identifying itself, so sites see a real browser version and who is asking.
pub(super) fn honest_user_agent(chrome_ua: &str) -> String {
    let base = if chrome_ua.is_empty() {
        "Mozilla/5.0 (compatible)".to_string()
    } else {
        chrome_ua.replace("HeadlessChrome", "Chrome")
    };
    format!(
        "{base} mollendorff-ref/{} (+https://github.com/mollendorff-ai/ref)",
        env!("CARGO_PKG_VERSION")
    )
}

pub(super) fn to_cookie_param(c: &Cookie) -> Option<CookieParam> {
    let mut b = CookieParam::builder()
        .name(c.name.clone())
        .value(c.value.clone())
        .domain(c.domain.clone())
        .path(c.path.clone())
        .secure(c.secure)
        .http_only(c.http_only);
    if let Some(exp) = c.expires {
        #[allow(clippy::cast_precision_loss)]
        let exp = exp as f64;
        b = b.expires(TimeSinceEpoch::new(exp));
    }
    b.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_is_the_first_known_path_that_exists() {
        // Chrome is a requirement of ref, so of its tests (ADR-004)
        assert!(detect_chrome_path().unwrap().exists());
        assert_eq!(
            first_existing(&["/no/chrome", "/"]),
            Some(PathBuf::from("/"))
        );
        assert_eq!(first_existing(&["/no/chrome"]), None);
    }

    #[test]
    fn a_profile_open_in_another_chrome_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Chrome's lock names its host and process: this process is alive
        let lock = format!("host-{}", std::process::id());
        std::os::unix::fs::symlink(&lock, dir.path().join("SingletonLock")).unwrap();
        let err = prepare_saved_profile(dir.path(), None)
            .unwrap_err()
            .to_string();
        let open = format!("is open in another Chrome (process {})", std::process::id());
        assert!(err.contains(&open), "{err}");
    }

    // The sweep reads /proc, so it exists on Linux only
    #[cfg(target_os = "linux")]
    #[test]
    fn the_sweep_ends_the_chrome_and_profile_of_a_dead_ref_only() {
        let temp = tempfile::tempdir().unwrap();
        // A process id that is free now: a ref killed before its clean-up
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        gone.wait().unwrap();
        let dead = temp.path().join(format!("ref-chrome-{}-1", gone.id()));
        let own = temp
            .path()
            .join(format!("ref-chrome-{}-2", std::process::id()));
        let live = temp.path().join("ref-chrome-1-3");
        let other = temp.path().join("not-a-profile");
        for dir in [&dead, &own, &live, &other] {
            std::fs::create_dir(dir).unwrap();
        }
        // Its Chrome, still running on the profile
        let mut chrome = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 60; true")
            .arg(format!("--user-data-dir={}", dead.display()))
            .spawn()
            .unwrap();

        sweep_orphans_in(temp.path());
        assert!(!dead.exists(), "the dead ref's profile was kept");
        let ended = chrome.wait().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&ended),
            Some(9)
        );
        for kept in [&own, &live, &other] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        // A folder that cannot be read is nothing to sweep
        sweep_orphans_in(&temp.path().join("missing"));
    }

    #[test]
    fn test_chrome_switches_are_bare_names() {
        for (key, value) in CHROME_SWITCHES {
            assert!(!key.starts_with('-') && !key.contains('='), "{key}");
            assert!(!value.contains(' '), "{value}");
        }
        assert!(DISABLED_FEATURES.iter().all(|f| !f.contains([',', ' '])));
    }

    #[test]
    fn test_want_sandbox() {
        assert!(want_sandbox(None, false));
        assert!(!want_sandbox(None, true));
        assert!(!want_sandbox(Some("1"), false));
        assert!(!want_sandbox(Some("TRUE"), false));
        assert!(want_sandbox(Some("0"), false));
    }

    #[test]
    fn test_uid_is_root() {
        assert!(uid_is_root("Name:\tref\nUid:\t1000\t0\t0\t0\n"));
        assert!(!uid_is_root("Uid:\t1000\t1000\t1000\t1000\n"));
        assert!(!uid_is_root("Name:\tref\n"));
    }

    #[test]
    fn test_validate_user_agent() {
        assert_eq!(
            validate_user_agent("  Jane Doe jane@example.com ").unwrap(),
            "Jane Doe jane@example.com"
        );
        assert!(validate_user_agent("   ").is_err());
        assert!(validate_user_agent("a\r\nX-Evil: 1").is_err());
    }

    #[test]
    fn test_honest_user_agent() {
        let ua = honest_user_agent(
            "Mozilla/5.0 (X11; Linux x86_64) HeadlessChrome/142.0.0.0 Safari/537.36",
        );
        assert!(ua.contains(" Chrome/142.0.0.0 "));
        assert!(!ua.contains("Headless"));
        assert!(ua.contains("mollendorff-ref/"));
        assert!(honest_user_agent("").starts_with("Mozilla/5.0 (compatible) mollendorff-ref/"));
    }

    #[test]
    fn removing_a_profile_folder_waits_only_while_it_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        std::fs::create_dir_all(profile.join("inner")).unwrap();
        let started = std::time::Instant::now();
        remove_dir_retrying(&profile);
        assert!(!profile.exists());
        // Gone at once: no waiting
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        let again = std::time::Instant::now();
        remove_dir_retrying(&profile);
        assert!(again.elapsed() < std::time::Duration::from_millis(250));
    }

    #[test]
    fn a_folder_that_cannot_be_removed_is_tried_three_times() {
        // A plain file is not a folder, so removing it as one always fails
        let dir = tempfile::tempdir().unwrap();
        let stuck = dir.path().join("stuck");
        std::fs::write(&stuck, "x").unwrap();
        let started = std::time::Instant::now();
        remove_dir_retrying(&stuck);
        let took = started.elapsed();
        assert!(stuck.exists());
        // Three tries, two waits of 300 ms between them
        assert!(took >= std::time::Duration::from_millis(600), "{took:?}");
        assert!(took < std::time::Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn only_the_profiles_of_pools_that_are_gone_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (live, gone) = (dir.path().join("live"), dir.path().join("gone"));
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&gone).unwrap();
        let live_owner = std::sync::Arc::new(());
        track_profile(&live, &live_owner);
        track_profile(&gone, &std::sync::Arc::new(()));

        remove_open_profiles();
        assert!(live.exists(), "the profile of a pool in use was removed");
        assert!(
            !gone.exists(),
            "the profile of a pool that is gone was kept"
        );
        assert!(!is_tracked(&gone));
        // A folder still on disk stays tracked
        untrack_profile(&live);
        assert!(is_tracked(&live));
        std::fs::remove_dir(&live).unwrap();
        untrack_profile(&live);
        assert!(!is_tracked(&live));
    }

    #[test]
    fn test_profile_owner() {
        assert_eq!(profile_owner("ref-chrome-4195-594839600"), Some(4195));
        assert_eq!(profile_owner("ref-chrome-x-1"), None);
        assert_eq!(profile_owner("ref-chrome-12"), None);
        assert_eq!(profile_owner("ref-chrome-12-abc"), None);
        assert_eq!(profile_owner("chromiumoxide-runner"), None);
        let own = unique_profile_dir();
        let name = own.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(profile_owner(&name), Some(std::process::id()));
    }

    #[test]
    fn test_unique_profile_dirs() {
        let a = unique_profile_dir();
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert_ne!(a, unique_profile_dir());
        assert!(a.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn test_to_cookie_param() {
        let c = Cookie {
            domain: ".example.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            expires: Some(1_893_456_000),
            name: "sid".into(),
            value: "abc".into(),
        };
        let p = to_cookie_param(&c).unwrap();
        assert_eq!(p.name, "sid");
        assert_eq!(p.domain.as_deref(), Some(".example.com"));
        assert!(p.expires.is_some());
        let session = Cookie { expires: None, ..c };
        assert!(to_cookie_param(&session).unwrap().expires.is_none());
    }
}
