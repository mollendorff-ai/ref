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
    // A symlink that points nowhere would be followed on write, possibly to
    // a file outside the workspace
    let dangling = joined
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
        && !joined.exists();
    if dangling {
        return Err(format!(
            "{raw}: refusing a symlink that points to a missing file"
        ));
    }
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

/// Check that `path` may receive a references file written by ref.
///
/// The name must end in `.yaml` or `.yml`. An existing file must be empty or
/// already be a references file, so a scan can never replace source code or
/// other documents, and it must not be a hard link (whose other name could
/// sit outside the workspace).
///
/// # Errors
///
/// Returns a message naming the reason the file is refused.
pub fn check_references_output(path: &Path) -> Result<(), String> {
    let shown = path.display();
    let yaml = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
    if !yaml {
        return Err(format!("{shown}: output must be a .yaml or .yml file"));
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(()); // a new file
    };
    if meta.is_file() && hard_link_count(&meta) > 1 {
        return Err(format!(
            "{shown}: refusing to write a file with more than one hard link"
        ));
    }
    // Unreadable files and folders count as "not a references file"
    let is_references = std::fs::read_to_string(path).is_ok_and(|text| {
        text.trim().is_empty()
            || serde_yaml::from_str::<crate::schema::ReferencesFile>(&text).is_ok()
    });
    if is_references {
        Ok(())
    } else {
        Err(format!(
            "{shown}: exists and is not a references file; refusing to overwrite it"
        ))
    }
}

#[cfg(unix)]
fn hard_link_count(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn hard_link_count(_meta: &std::fs::Metadata) -> u64 {
    1
}

/// Write a file by writing a temporary sibling and renaming it into place.
///
/// The rename replaces the directory entry, so a reader never sees a half
/// written file and a hard link to the old file is left untouched.
///
/// # Errors
///
/// Returns the I/O error from writing or renaming.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.ref-tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
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

    #[cfg(unix)]
    #[test]
    fn dangling_symlinks_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("escaped.yaml"),
            dir.path().join("out.yaml"),
        )
        .unwrap();
        let err = check_workspace_path("out.yaml", dir.path()).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn files_reached_through_a_symlinked_folder_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.md"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("docs")).unwrap();
        assert!(check_workspace_path("docs/secret.md", dir.path()).is_err());
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
    fn references_output_must_be_yaml_and_a_references_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);

        assert!(check_references_output(&p("new.yaml")).is_ok());
        assert!(check_references_output(&p("new.YML")).is_ok());
        assert!(check_references_output(&p("important.rs"))
            .unwrap_err()
            .contains(".yaml"));

        fs::write(p("empty.yaml"), "  \n").unwrap();
        assert!(check_references_output(&p("empty.yaml")).is_ok());

        let refs = "meta:\n  created: 2026-11-06\n  last_verified: null\n  tool: ref\n  total_links: 0\nreferences: []\n";
        fs::write(p("refs.yaml"), refs).unwrap();
        assert!(check_references_output(&p("refs.yaml")).is_ok());

        fs::write(p("config.yaml"), "server:\n  port: 8080\n").unwrap();
        let err = check_references_output(&p("config.yaml")).unwrap_err();
        assert!(err.contains("not a references file"), "{err}");

        fs::create_dir(p("dir.yaml")).unwrap();
        assert!(check_references_output(&p("dir.yaml")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_outputs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.yaml"), "").unwrap();
        fs::hard_link(dir.path().join("a.yaml"), dir.path().join("b.yaml")).unwrap();
        let err = check_references_output(&dir.path().join("b.yaml")).unwrap_err();
        assert!(err.contains("hard link"), "{err}");
    }

    #[test]
    fn atomic_writes_replace_the_entry_not_the_linked_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.yaml");
        write_atomic(&target, b"first").unwrap();
        #[cfg(unix)]
        {
            let other = dir.path().join("other.yaml");
            fs::hard_link(&target, &other).unwrap();
            write_atomic(&target, b"second").unwrap();
            assert_eq!(fs::read_to_string(&other).unwrap(), "first");
        }
        assert_ne!(fs::read_to_string(&target).unwrap(), "");
        let err = write_atomic(&dir.path().join("missing/out.yaml"), b"x");
        assert!(err.is_err());

        // Rename onto a non-empty folder fails; the temporary file is removed
        let busy = dir.path().join("busy.yaml");
        fs::create_dir(&busy).unwrap();
        fs::write(busy.join("inside"), "x").unwrap();
        assert!(write_atomic(&busy, b"x").is_err());
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("ref-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn patterns_must_stay_relative() {
        assert!(check_workspace_pattern("docs/**/*.md").is_ok());
        assert!(check_workspace_pattern("/etc/*.conf").is_err());
        assert!(check_workspace_pattern("../*.md").is_err());
    }
}
