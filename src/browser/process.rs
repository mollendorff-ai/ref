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

    for path in candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Profile folders of pools that are still open, so an interrupted run can
/// remove them (see [`remove_open_profiles`]).
static OPEN_PROFILES: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

pub(super) fn track_profile(dir: &Path, open: bool) {
    if let Ok(mut dirs) = OPEN_PROFILES.lock() {
        dirs.retain(|d| d != dir);
        if open {
            dirs.push(dir.to_path_buf());
        }
    }
}

/// Remove the temporary Chrome profiles of pools that were not closed.
/// Called when ref is interrupted (Ctrl-C or SIGTERM).
pub fn remove_open_profiles() {
    if let Ok(mut dirs) = OPEN_PROFILES.lock() {
        if dirs.is_empty() {
            return;
        }
        // A killed Chrome can still write to its profile for a moment
        // while it exits, so remove, wait, and remove again.
        for _ in 0..3 {
            for dir in dirs.iter() {
                let _ = std::fs::remove_dir_all(dir);
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        dirs.retain(|d| d.exists());
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
        .map_err(|e| anyhow::anyhow!("Browser config error: {e}"))?;

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
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = profile_owner(&name) else {
            continue;
        };
        if pid == std::process::id() || Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let dir = entry.path();
        let killed = kill_chrome_using(&dir);
        // A killed Chrome can still write to its profile while it exits
        for _ in 0..if killed { 3 } else { 1 } {
            let _ = std::fs::remove_dir_all(&dir);
            if killed {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn sweep_orphans() {}

/// Kill every process whose command line uses `profile` as its user-data-dir;
/// true when any was found
#[cfg(target_os = "linux")]
fn kill_chrome_using(profile: &Path) -> bool {
    let needle = format!("--user-data-dir={}", profile.display());
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    let mut killed = false;
    for proc_entry in procs.flatten() {
        let pid = proc_entry.file_name().to_string_lossy().into_owned();
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let cmdline = std::fs::read(proc_entry.path().join("cmdline")).unwrap_or_default();
        let uses_profile = cmdline
            .split(|b| *b == 0)
            .any(|arg| arg == needle.as_bytes());
        if let (true, Ok(pid)) = (uses_profile, pid.parse()) {
            kill_process(pid);
            killed = true;
        }
    }
    killed
}

/// The ref process id in a profile folder name `ref-chrome-<pid>-<nanos>`
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
    fn test_detect_chrome_path() {
        // This test verifies the function runs without panic
        // Actual detection depends on system Chrome installation
        let result = detect_chrome_path();
        // On CI without Chrome, this may be None - that's fine
        if let Some(path) = result {
            assert!(path.exists());
        }
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
