//! Input guards: which URLs ref may open and which paths MCP tools may touch.
//!
//! `fetch`, `check-links`, `verify-refs` and `refresh-data` open only
//! `http` and `https` URLs, so a `file://` link (or a prompt-injected one
//! sent through MCP) cannot read local files.
//! MCP tools read and write files only inside the server's working directory.

use std::path::{Component, Path, PathBuf};
use url::Url;

/// Accept only `http` and `https` URLs.
///
/// # Errors
///
/// Returns a short message naming the problem when the URL is malformed or
/// uses another scheme.
pub fn check_web_url(raw: &str) -> Result<(), String> {
    let url = Url::parse(raw).map_err(|e| format!("Invalid URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!(
            "Only http and https URLs are allowed (got {other}:)"
        )),
    }
}

/// Resolve a path given to an MCP tool and make sure it stays inside `root`.
///
/// Relative paths are taken from `root`. Paths with `..`, and absolute paths
/// or symlinks that lead outside `root`, are refused. The file itself does
/// not need to exist (it may be an output), but its parent folder must.
///
/// # Errors
///
/// Returns a message when the path leaves `root` or its folder is missing.
pub fn check_workspace_path(raw: &str, root: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("Working directory unavailable: {e}"))?;
    let path = Path::new(raw);
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(outside(raw));
    }
    let joined = root.join(path);
    // An existing path is resolved fully (following symlinks); a new file is
    // resolved through its parent folder, which must exist.
    let resolved = if joined.exists() {
        joined.canonicalize().ok()
    } else {
        joined
            .parent()
            .zip(joined.file_name())
            .and_then(|(parent, name)| parent.canonicalize().ok().map(|p| p.join(name)))
    };
    let Some(resolved) = resolved else {
        return Err(format!("{raw}: folder not found"));
    };
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(outside(raw))
    }
}

/// Check a glob pattern given to an MCP tool: relative, without `..`.
///
/// # Errors
///
/// Returns a message when the pattern is absolute or climbs out with `..`.
pub fn check_workspace_pattern(raw: &str) -> Result<(), String> {
    let path = Path::new(raw);
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(outside(raw));
    }
    Ok(())
}

fn outside(raw: &str) -> String {
    format!("{raw}: only paths inside the working directory are allowed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn web_urls_are_allowed() {
        assert!(check_web_url("https://example.com/a?b=c").is_ok());
        assert!(check_web_url("http://127.0.0.1:8080/").is_ok());
    }

    #[test]
    fn other_schemes_are_refused() {
        let err = check_web_url("file:///etc/passwd").unwrap_err();
        assert!(err.contains("file:"), "{err}");
        assert!(check_web_url("data:text/html,hi").is_err());
        assert!(check_web_url("chrome://settings").is_err());
        assert!(check_web_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn malformed_urls_are_refused() {
        assert!(check_web_url("not a url")
            .unwrap_err()
            .starts_with("Invalid URL"));
    }

    #[test]
    fn relative_paths_inside_root_resolve() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("refs.yaml"), "x").unwrap();
        fs::create_dir(dir.path().join("docs")).unwrap();

        let existing = check_workspace_path("refs.yaml", dir.path()).unwrap();
        assert!(existing.ends_with("refs.yaml"));

        let new_file = check_workspace_path("docs/out.yaml", dir.path()).unwrap();
        assert!(new_file.ends_with("docs/out.yaml"));
    }

    #[test]
    fn parent_dir_and_absolute_escapes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_workspace_path("../secret.yaml", dir.path()).is_err());
        assert!(check_workspace_path("/etc/passwd", dir.path()).is_err());
        assert!(check_workspace_path("/tmp/new-file.yaml", dir.path()).is_err());
    }

    #[test]
    fn absolute_path_inside_root_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        fs::write(&file, "x").unwrap();
        assert!(check_workspace_path(file.to_str().unwrap(), dir.path()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_leading_outside_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc", dir.path().join("etc-link")).unwrap();
        assert!(check_workspace_path("etc-link/hostname", dir.path()).is_err());
    }

    #[test]
    fn missing_folder_or_root_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_workspace_path("nope/out.yaml", dir.path()).unwrap_err();
        assert!(err.contains("folder not found"), "{err}");

        let gone = dir.path().join("gone");
        let err = check_workspace_path("a.yaml", &gone).unwrap_err();
        assert!(err.starts_with("Working directory unavailable"), "{err}");
    }

    #[test]
    fn root_itself_has_no_file_name() {
        // "" joins to the root, which exists and is inside itself
        let dir = tempfile::tempdir().unwrap();
        assert!(check_workspace_path("", dir.path()).is_ok());
        assert!(check_workspace_path(".", dir.path()).is_ok());
    }

    #[test]
    fn patterns_must_stay_relative() {
        assert!(check_workspace_pattern("docs/**/*.md").is_ok());
        assert!(check_workspace_pattern("/etc/*.conf").is_err());
        assert!(check_workspace_pattern("../*.md").is_err());
    }
}
