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
    /// [default: `REF_CHROME_PROFILE` when set, else ~/.ref/chrome-profile once
    /// `ref login` has created it, else fresh]
    #[arg(long, value_name = "FOLDER|fresh", value_parser = parse_profile)]
    pub profile: Option<ProfileChoice>,
}

impl ProfileArgs {
    /// The `--profile` value, else `REF_CHROME_PROFILE` (see [`resolve`])
    ///
    /// # Errors
    /// Returns an error when the variable holds a value that cannot be used.
    pub fn choice(&self) -> anyhow::Result<Option<ProfileChoice>> {
        resolve(self.profile.as_ref())
    }
}

/// The profile chosen on the command line: `flag` when given, else
/// `REF_CHROME_PROFILE` when it is set and not empty (an empty value counts as
/// not set, so `REF_CHROME_PROFILE= ref fetch ...` uses the default).
///
/// # Errors
/// Returns an error when the variable holds a value that cannot be used.
pub fn resolve(flag: Option<&ProfileChoice>) -> anyhow::Result<Option<ProfileChoice>> {
    if let Some(choice) = flag {
        return Ok(Some(choice.clone()));
    }
    match std::env::var(PROFILE_ENV) {
        Ok(value) if !value.trim().is_empty() => parse_profile(&value)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{PROFILE_ENV}: {e}")),
        _ => Ok(None),
    }
}

/// A `--profile` value
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileChoice {
    /// An empty temporary profile, deleted when the run ends
    Fresh,
    /// A saved profile folder, as an absolute path
    Folder(PathBuf),
}

/// Parse a `--profile` value: `fresh` (any case), or a folder, made absolute
/// against the current folder so the everyday-profile check sees where it is.
///
/// # Errors
/// Returns an error for an empty value, or when the current folder is unknown.
pub fn parse_profile(value: &str) -> Result<ProfileChoice, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("the profile is empty: give a folder or `fresh`".to_string());
    }
    if value.eq_ignore_ascii_case("fresh") {
        return Ok(ProfileChoice::Fresh);
    }
    std::path::absolute(value)
        .map(ProfileChoice::Folder)
        .map_err(|e| format!("{value}: {e}"))
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
    value: Option<&ProfileChoice>,
    default_saved: Option<&Path>,
    fresh: impl FnOnce() -> PathBuf,
) -> Profile {
    match value {
        Some(ProfileChoice::Fresh) => Profile::Fresh(fresh()),
        Some(ProfileChoice::Folder(dir)) => Profile::Saved(dir.clone()),
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

/// True when `dir` is (or is inside) one of Chrome's default user data folders,
/// also when reached through a symbolic link, and when `dir` does not exist yet
#[must_use]
pub fn is_chrome_default_dir(dir: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else {
        return false;
    };
    let dir = real_path(dir);
    CHROME_DEFAULT_DIRS
        .iter()
        .any(|rel| dir.starts_with(real_path(&home.join(rel))))
}

/// `path` with symbolic links resolved in the part of it that exists; the
/// rest (folders not made yet) is kept as written
fn real_path(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(real) = existing.canonicalize() {
            return missing.iter().rev().fold(real, |p, name| p.join(name));
        }
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            return path.to_path_buf();
        };
        missing.push(name);
        existing = parent;
    }
}

/// The process id in Chrome's `SingletonLock` link target (`<hostname>-<pid>`)
#[must_use]
pub fn lock_owner_pid(lock_target: &str) -> Option<u32> {
    lock_target.rsplit_once('-')?.1.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct WithProfile {
        #[command(flatten)]
        profile: ProfileArgs,
    }

    #[test]
    fn profile_option_takes_a_folder_or_fresh() {
        let folder = WithProfile::try_parse_from(["ref", "--profile", "/work/p"]).unwrap();
        assert_eq!(
            folder.profile.profile,
            Some(ProfileChoice::Folder(PathBuf::from("/work/p")))
        );
        let fresh = WithProfile::try_parse_from(["ref", "--profile", " Fresh "]).unwrap();
        assert_eq!(fresh.profile.profile, Some(ProfileChoice::Fresh));
        let empty = WithProfile::try_parse_from(["ref", "--profile", " "]);
        assert!(empty
            .err()
            .unwrap()
            .to_string()
            .contains("profile is empty"));
    }

    #[test]
    fn relative_profile_folders_are_made_absolute() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(
            parse_profile("work/p"),
            Ok(ProfileChoice::Folder(here.join("work/p")))
        );
    }

    /// Set in the child process `profile_option_beats_the_environment_variable` starts
    const CHILD: &str = "REF_TEST_PROFILE_CHILD";

    #[test]
    fn profile_option_beats_the_environment_variable() {
        // Runs again as a child process with the variable set: setting it in
        // this process would race the other tests
        let given = ProfileChoice::Fresh;
        match std::env::var(CHILD).as_deref() {
            Ok("folder") => {
                let folder = ProfileChoice::Folder(PathBuf::from("/from/env"));
                assert_eq!(resolve(None).unwrap(), Some(folder));
                assert_eq!(resolve(Some(&given)).unwrap(), Some(ProfileChoice::Fresh));
            }
            Ok("relative") => {
                let here = std::env::current_dir().unwrap();
                let folder = ProfileChoice::Folder(here.join("work/p"));
                assert_eq!(resolve(None).unwrap(), Some(folder));
            }
            Ok(_) => {
                // An empty variable counts as not set
                assert_eq!(resolve(None).unwrap(), None);
                assert_eq!(ProfileArgs::default().choice().unwrap(), None);
            }
            Err(_) => {
                for (mode, value) in [
                    ("folder", "/from/env"),
                    ("relative", "work/p"),
                    ("empty", " "),
                ] {
                    let status = std::process::Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "profile::tests::profile_option_beats_the_environment_variable",
                        ])
                        .env(CHILD, mode)
                        .env(PROFILE_ENV, value)
                        .status()
                        .unwrap();
                    assert!(
                        status.success(),
                        "child run with {PROFILE_ENV}={value:?} failed"
                    );
                }
            }
        }
    }

    fn fresh() -> PathBuf {
        PathBuf::from("/tmp/ref-chrome-1-2")
    }

    #[test]
    fn fresh_wins_over_an_existing_saved_profile() {
        let saved = tempfile::tempdir().unwrap();
        let p = choose(Some(&ProfileChoice::Fresh), Some(saved.path()), fresh);
        assert_eq!(p, Profile::Fresh(fresh()));
        assert!(!p.is_saved());
        assert_eq!(p.path(), Path::new("/tmp/ref-chrome-1-2"));
    }

    #[test]
    fn a_folder_is_a_saved_profile() {
        let folder = ProfileChoice::Folder(PathBuf::from("/work/ref-profile"));
        let p = choose(Some(&folder), None, fresh);
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
        assert_eq!(choose(None, Some(&dir), fresh), Profile::Saved(dir.clone()));
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

    #[cfg(unix)]
    #[test]
    fn chrome_default_dir_behind_a_symlink_is_recognised() {
        let home = tempfile::tempdir().unwrap();
        let everyday = home.path().join(".config/google-chrome");
        std::fs::create_dir_all(&everyday).unwrap();
        let link = home.path().join("innocent-looking");
        std::os::unix::fs::symlink(&everyday, &link).unwrap();
        assert!(is_chrome_default_dir(&link, Some(home.path())));
        assert!(!is_chrome_default_dir(
            &home.path().join("elsewhere"),
            Some(home.path())
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_new_folder_under_a_symlinked_home_is_recognised() {
        let disk = tempfile::tempdir().unwrap();
        let real_home = disk.path().join("data/home/me");
        std::fs::create_dir_all(real_home.join(".config/google-chrome")).unwrap();
        let home = disk.path().join("home-link");
        std::os::unix::fs::symlink(&real_home, &home).unwrap();

        // Not made yet, so it cannot be resolved as a whole
        let new_profile = home.join(".config/google-chrome/NewProfile/Deeper");
        assert!(is_chrome_default_dir(&new_profile, Some(&home)));
        assert!(!is_chrome_default_dir(&home.join("other/new"), Some(&home)));
    }

    #[test]
    fn a_path_with_no_existing_part_is_kept_as_written() {
        let nowhere = Path::new("no-such-folder-for-ref-tests/inner");
        assert_eq!(real_path(nowhere), nowhere);
    }

    #[test]
    fn lock_owner_is_parsed() {
        assert_eq!(lock_owner_pid("mac-mini.local-4321"), Some(4321));
        assert_eq!(lock_owner_pid("host-name-17"), Some(17));
        assert_eq!(lock_owner_pid("nopid"), None);
        assert_eq!(lock_owner_pid("host-abc"), None);
    }
}
