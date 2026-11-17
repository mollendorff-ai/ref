//! Which Chrome profile (user data folder) ref runs with.
//!
//! - A **saved profile** keeps your sign-ins: you open it once with
//!   `ref login`, sign in to your sites and pass any "I am human" checks
//!   yourself, and every later run reuses those sessions. ref never deletes it.
//! - A **fresh profile** is a new empty folder per run, deleted afterwards.
//!
//! Choice, in order: `--profile fresh` → fresh; `--profile <folder>` → that
//! saved profile; otherwise the default saved profile (`~/.ref/chrome-profile`)
//! when `ref login` has created it; otherwise fresh.
//! Without `--profile`, the `REF_CHROME_PROFILE` environment variable gives the value.

use std::path::{Path, PathBuf};

/// Environment variable that selects the profile when `--profile` is not given
pub const PROFILE_ENV: &str = "REF_CHROME_PROFILE";

/// The `--profile` option of the commands that start Chrome
#[derive(clap::Args, Debug, Clone, Default)]
pub struct ProfileArgs {
    /// Chrome profile: a folder (a saved profile, kept between runs) or `fresh`
    /// (an empty temporary one, deleted afterwards)
    /// [default: ~/.ref/chrome-profile once `ref login` has created it, else fresh]
    #[arg(long, env = PROFILE_ENV, value_name = "FOLDER|fresh")]
    pub profile: Option<String>,
}

/// The profile a run uses
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Profile {
    /// Temporary folder, deleted when the run ends
    Fresh(PathBuf),
    /// Your own folder with your sign-ins, never deleted
    Saved(PathBuf),
}

impl Profile {
    /// The user data folder passed to Chrome
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Fresh(p) | Self::Saved(p) => p,
        }
    }

    /// True for a saved profile (sign-ins kept, never deleted)
    #[must_use]
    pub fn is_saved(&self) -> bool {
        matches!(self, Self::Saved(_))
    }
}

/// Default saved profile: `<home>/.ref/chrome-profile`
#[must_use]
pub fn default_saved_dir(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|h| h.join(".ref").join("chrome-profile"))
}

/// The user's home folder from `HOME` (or `USERPROFILE` on Windows)
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Pick the profile from the `--profile` value and the default saved folder.
///
/// `fresh` makes a new temporary folder name when a fresh profile is chosen.
#[must_use]
pub fn choose(
    value: Option<&str>,
    default_saved: Option<&Path>,
    fresh: impl FnOnce() -> PathBuf,
) -> Profile {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) if v.eq_ignore_ascii_case("fresh") => Profile::Fresh(fresh()),
        Some(dir) => Profile::Saved(PathBuf::from(dir)),
        None => match default_saved {
            Some(dir) if dir.is_dir() => Profile::Saved(dir.to_path_buf()),
            _ => Profile::Fresh(fresh()),
        },
    }
}

/// Chrome's own default user data folders, relative to the home folder.
///
/// Since Chrome 136, Chrome refuses remote control (which ref needs) on these
/// folders, to protect the cookies of your everyday browser. Sign-ins for ref
/// live in a separate saved profile instead (see `ref login`).
const CHROME_DEFAULT_DIRS: [&str; 6] = [
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Chromium",
    ".config/google-chrome",
    ".config/chromium",
    "AppData/Local/Google/Chrome/User Data",
    "AppData/Local/Chromium/User Data",
];

/// True when `dir` is (or is inside) one of Chrome's default user data folders
#[must_use]
pub fn is_chrome_default_dir(dir: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else {
        return false;
    };
    CHROME_DEFAULT_DIRS
        .iter()
        .any(|rel| dir.starts_with(home.join(rel)))
}

/// The process id in Chrome's `SingletonLock` link target (`<hostname>-<pid>`)
#[must_use]
pub fn lock_owner_pid(lock_target: &str) -> Option<u32> {
    lock_target.rsplit_once('-')?.1.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct WithProfile {
        #[command(flatten)]
        profile: ProfileArgs,
    }

    #[test]
    fn profile_option_takes_a_folder_or_fresh() {
        let folder = WithProfile::try_parse_from(["ref", "--profile", "/work/p"]).unwrap();
        assert_eq!(folder.profile.profile.as_deref(), Some("/work/p"));
        let fresh = WithProfile::try_parse_from(["ref", "--profile", "fresh"]).unwrap();
        assert_eq!(fresh.profile.profile.as_deref(), Some("fresh"));
    }

    #[test]
    fn profile_option_falls_back_to_the_environment_variable() {
        // Checked on the definition: setting the variable here would race other tests
        let command = WithProfile::command();
        let arg = command
            .get_arguments()
            .find(|a| a.get_id() == "profile")
            .unwrap();
        assert_eq!(arg.get_env(), Some(std::ffi::OsStr::new(PROFILE_ENV)));
        assert_eq!(arg.get_long(), Some("profile"));
    }

    fn fresh() -> PathBuf {
        PathBuf::from("/tmp/ref-chrome-1-2")
    }

    #[test]
    fn env_fresh_wins() {
        let saved = tempfile::tempdir().unwrap();
        let p = choose(Some(" FRESH "), Some(saved.path()), fresh);
        assert_eq!(p, Profile::Fresh(fresh()));
        assert!(!p.is_saved());
        assert_eq!(p.path(), Path::new("/tmp/ref-chrome-1-2"));
    }

    #[test]
    fn env_folder_is_a_saved_profile() {
        let p = choose(Some("/work/ref-profile"), None, fresh);
        assert_eq!(p, Profile::Saved(PathBuf::from("/work/ref-profile")));
        assert!(p.is_saved());
        assert_eq!(p.path(), Path::new("/work/ref-profile"));
    }

    #[test]
    fn default_saved_profile_is_used_once_it_exists() {
        let home = tempfile::tempdir().unwrap();
        let dir = default_saved_dir(Some(home.path())).unwrap();
        assert_eq!(choose(None, Some(&dir), fresh), Profile::Fresh(fresh()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            choose(Some(""), Some(&dir), fresh),
            Profile::Saved(dir.clone())
        );
        assert_eq!(choose(None, None, fresh), Profile::Fresh(fresh()));
    }

    #[test]
    fn default_saved_dir_needs_a_home() {
        assert_eq!(default_saved_dir(None), None);
        assert_eq!(
            default_saved_dir(Some(Path::new("/home/me"))),
            Some(PathBuf::from("/home/me/.ref/chrome-profile"))
        );
    }

    #[test]
    fn home_dir_comes_from_the_environment() {
        // HOME is set in every test environment ref supports
        assert!(home_dir().is_some());
    }

    #[test]
    fn chrome_default_dirs_are_recognised() {
        let home = Path::new("/Users/me");
        assert!(is_chrome_default_dir(
            &home.join("Library/Application Support/Google/Chrome"),
            Some(home)
        ));
        assert!(is_chrome_default_dir(
            &home.join("Library/Application Support/Google/Chrome/Default"),
            Some(home)
        ));
        assert!(!is_chrome_default_dir(
            &home.join(".ref/chrome-profile"),
            Some(home)
        ));
        assert!(!is_chrome_default_dir(
            &home.join(".config/google-chrome"),
            None
        ));
    }

    #[test]
    fn lock_owner_is_parsed() {
        assert_eq!(lock_owner_pid("mac-mini.local-4321"), Some(4321));
        assert_eq!(lock_owner_pid("host-name-17"), Some(17));
        assert_eq!(lock_owner_pid("nopid"), None);
        assert_eq!(lock_owner_pid("host-abc"), None);
    }
}
