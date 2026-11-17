//! scan command: Extract URLs from markdown files
//!
//! Scans markdown files, extracts URLs, and creates/updates references.yaml.

use crate::schema::{Meta, Reference, ReferencesFile, Status};
use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Args)]
pub struct ScanArgs {
    /// Files or glob patterns to scan
    #[arg(required = true)]
    pub files: Vec<PathBuf>,

    /// Output file (default: references.yaml)
    #[arg(short, long, default_value = "references.yaml")]
    pub output: PathBuf,

    /// Overwrite the output file instead of merging with what it already holds
    #[arg(long = "no-merge", action = clap::ArgAction::SetFalse)]
    pub merge: bool,

    /// Merge with the existing file (the default; kept for older scripts)
    #[arg(long = "merge", hide = true)]
    pub merge_compat: bool,
}

#[derive(Debug, Serialize)]
pub struct ScanOutput {
    pub file: String,
    pub scanned_files: usize,
    pub total_urls: usize,
    pub new_urls: usize,
    pub updated_urls: usize,
}

/// A URL found in a markdown file with optional title
#[derive(Debug, Clone)]
struct FoundUrl {
    url: String,
    title: Option<String>,
    source_file: String,
}

/// # Errors
/// Returns an error if file patterns cannot be expanded, files cannot be read, or output cannot be written.
pub async fn run_scan(args: ScanArgs, out: &mut dyn std::io::Write) -> Result<()> {
    let output = scan_files(args).await?;
    writeln!(out, "{}", serde_json::to_string(&output)?)?;
    Ok(())
}

/// Core scan logic: expand files, extract URLs, merge, write output file.
///
/// # Errors
///
/// Returns an error if file patterns cannot be expanded, files cannot be read, or output cannot be written.
pub(crate) async fn scan_files(args: ScanArgs) -> Result<ScanOutput> {
    crate::guard::check_references_output(&args.output).map_err(|e| anyhow::anyhow!(e))?;
    let files = expand_files(&args.files).await?;

    if files.is_empty() {
        return Ok(ScanOutput {
            file: args.output.display().to_string(),
            scanned_files: 0,
            total_urls: 0,
            new_urls: 0,
            updated_urls: 0,
        });
    }

    let mut all_urls: Vec<FoundUrl> = Vec::new();
    for file in &files {
        let content = tokio::fs::read_to_string(file)
            .await
            .with_context(|| format!("Failed to read {}", file.display()))?;

        let found = extract_markdown_urls(&content, file);
        all_urls.extend(found);
    }

    let mut url_map: HashMap<String, Reference> = HashMap::new();

    for found in &all_urls {
        let entry = url_map.entry(found.url.clone()).or_insert_with(|| {
            let categories = infer_categories(&found.source_file);
            Reference {
                url: found.url.clone(),
                title: found.title.clone().unwrap_or_else(|| found.url.clone()),
                categories,
                cited_in: Vec::new(),
                status: Status::Pending,
                verified: None,
                notes: None,
            }
        });

        if !entry.cited_in.contains(&found.source_file) {
            entry.cited_in.push(found.source_file.clone());
        }

        if let Some(title) = &found.title {
            if entry.title == entry.url && title != &entry.url {
                entry.title.clone_from(title);
            }
        }
    }

    let (mut refs_file, _existing_count) = if args.merge && args.output.exists() {
        let content = tokio::fs::read_to_string(&args.output).await?;
        let existing: ReferencesFile = serde_yaml::from_str(&content)?;
        let count = existing.references.len();
        (existing, count)
    } else {
        let now = Utc::now().format("%Y-%m-%d").to_string();
        (
            ReferencesFile {
                meta: Meta {
                    created: now,
                    last_verified: None,
                    tool: "ref".to_string(),
                    total_links: 0,
                },
                references: Vec::new(),
            },
            0,
        )
    };

    let mut existing_urls: HashMap<String, usize> = HashMap::new();
    for (i, r) in refs_file.references.iter().enumerate() {
        existing_urls.insert(r.url.clone(), i);
    }

    let mut new_count = 0;
    let mut updated_count = 0;

    for (url, new_ref) in url_map {
        if let Some(&idx) = existing_urls.get(&url) {
            let existing = &mut refs_file.references[idx];
            for cited in &new_ref.cited_in {
                if !existing.cited_in.contains(cited) {
                    existing.cited_in.push(cited.clone());
                    updated_count += 1;
                }
            }
            if existing.title == existing.url && new_ref.title != new_ref.url {
                existing.title = new_ref.title;
            }
        } else {
            refs_file.references.push(new_ref);
            new_count += 1;
        }
    }

    refs_file.meta.total_links = refs_file.references.len();
    refs_file.references.sort_by(|a, b| a.url.cmp(&b.url));

    let yaml = serde_yaml::to_string(&refs_file)?;
    crate::guard::write_atomic(&args.output, yaml.as_bytes())
        .with_context(|| format!("Failed to write {}", args.output.display()))?;

    Ok(ScanOutput {
        file: args.output.display().to_string(),
        scanned_files: files.len(),
        total_urls: refs_file.references.len(),
        new_urls: new_count,
        updated_urls: updated_count,
    })
}

/// Expand file patterns to actual file paths
pub(crate) async fn expand_files(patterns: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for pattern in patterns {
        let pattern_str = pattern.to_string_lossy();

        if pattern_str.contains('*') {
            // Use glob for patterns
            for entry in glob::glob(&pattern_str)? {
                let path = entry?;
                if path.is_file() {
                    files.push(path);
                }
            }
        } else if pattern.is_file() {
            files.push(pattern.clone());
        } else if pattern.is_dir() {
            // Scan directory for markdown files
            let mut entries = tokio::fs::read_dir(pattern).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let markdown = path
                    .extension()
                    .is_some_and(|ext| ext == "md" || ext == "markdown");
                if markdown && path.is_file() {
                    files.push(path);
                }
            }
        }
    }

    Ok(files)
}

/// Markdown links: `[title](url)`
static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\((https?://[^\s\)]+)\)").expect("constant regex"));

/// Bare http(s) URLs
static BARE_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s\)>\]"'`]+"#).expect("constant regex"));

/// Extract URLs from markdown content
fn extract_markdown_urls(content: &str, source_file: &Path) -> Vec<FoundUrl> {
    let mut found = Vec::new();
    let source = source_file.to_string_lossy().to_string();

    // Match markdown links: [title](url)
    for cap in MD_LINK_RE.captures_iter(content) {
        let title = cap[1].to_string();
        let url = cap[2].to_string();
        // Clean trailing punctuation from URL
        let url = url
            .trim_end_matches([',', '.', ')', ']', ';', ':'])
            .to_string();

        found.push(FoundUrl {
            url,
            title: Some(title),
            source_file: source.clone(),
        });
    }

    // Match bare URLs (not already in markdown links)
    for mat in BARE_URL_RE.find_iter(content) {
        let url = mat.as_str();
        let url = url
            .trim_end_matches([',', '.', ')', ']', ';', ':'])
            .to_string();

        // Skip if already found as markdown link
        if !found.iter().any(|f| f.url == url) {
            found.push(FoundUrl {
                url,
                title: None,
                source_file: source.clone(),
            });
        }
    }

    found
}

/// Infer categories from file path
fn infer_categories(path: &str) -> Vec<String> {
    let mut categories = Vec::new();

    let lower = path.to_lowercase();

    // Infer from directory structure (check for dir names with or without slashes)
    if lower.contains("/docs/") || lower.contains("/doc/") || lower.starts_with("docs/") {
        categories.push("documentation".to_string());
    }
    if lower.contains("/research/") || lower.starts_with("research/") {
        categories.push("research".to_string());
    }
    if lower.contains("/adr/") || lower.starts_with("adr/") {
        categories.push("architecture".to_string());
    }
    if lower.contains("/api/") || lower.starts_with("api/") {
        categories.push("api".to_string());
    }
    if lower.contains("/test") || lower.starts_with("test") {
        categories.push("testing".to_string());
    }

    // Infer from filename
    if lower.contains("readme") {
        categories.push("readme".to_string());
    }
    if lower.contains("changelog") {
        categories.push("changelog".to_string());
    }
    if lower.contains("license") {
        categories.push("legal".to_string());
    }

    // Default category if none found
    if categories.is_empty() {
        categories.push("general".to_string());
    }

    categories
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::ref_cmd;
    use std::process::ExitCode;

    /// A folder with docs and a README holding links, to scan
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(
            root.join("README.md"),
            "See [the guide](https://example.com/guide) and https://example.com/bare.\n",
        )
        .unwrap();
        std::fs::write(
            root.join("docs/api.md"),
            "The [guide](https://example.com/guide) again, and https://example.com/api.\n",
        )
        .unwrap();
        std::fs::write(root.join("docs/notes.txt"), "https://example.com/ignored").unwrap();
        // A file without an extension is not markdown either
        std::fs::write(root.join("docs/Makefile"), "https://example.com/ignored").unwrap();
        dir
    }

    fn scan_json(out: &str) -> serde_json::Value {
        serde_json::from_str(out).expect("scan printed JSON")
    }

    fn read_refs(path: &Path) -> ReferencesFile {
        serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn files_folders_and_globs_are_scanned_and_urls_merged() {
        let dir = project();
        let root = dir.path();
        let out_file = root.join("references.yaml");
        let (readme, docs, glob) = (
            root.join("README.md"),
            root.join("docs"),
            root.join("docs/*.md"),
        );
        let line = [
            "scan",
            readme.to_str().unwrap(),
            docs.to_str().unwrap(),
            glob.to_str().unwrap(),
            "--output",
            out_file.to_str().unwrap(),
        ];
        let (code, out, _) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::SUCCESS);
        let json = scan_json(&out);
        // The .txt file is not markdown; the folder and the glob find api.md twice
        assert_eq!(json["total_urls"], 3, "{out}");
        assert_eq!(json["new_urls"], 3);

        let refs = read_refs(&out_file);
        assert_eq!(refs.meta.total_links, 3);
        let guide = refs
            .references
            .iter()
            .find(|r| r.url.ends_with("/guide"))
            .unwrap();
        assert_eq!(guide.title, "the guide");
        assert_eq!(guide.cited_in.len(), 2, "cited in README and api.md");
        let readme = "readme".to_string();
        assert!(guide.categories.contains(&readme), "{:?}", guide.categories);
        let bare = refs
            .references
            .iter()
            .find(|r| r.url.ends_with("/bare"))
            .unwrap();
        assert_eq!(bare.title, bare.url, "a bare URL is its own title");
    }

    #[tokio::test]
    async fn scanning_again_merges_with_what_the_file_holds() {
        let dir = project();
        let root = dir.path();
        let out_file = root.join("references.yaml");
        let readme = root.join("README.md");
        let first = [
            "scan",
            readme.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
        ];
        ref_cmd(&first).await;

        // A new file cites the bare URL under a title and adds one more link
        std::fs::write(
            root.join("more.md"),
            "[Bare page](https://example.com/bare) and https://example.com/extra\n",
        )
        .unwrap();
        let more = root.join("more.md");
        let second = [
            "scan",
            more.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
        ];
        let (code, out, _) = ref_cmd(&second).await;
        assert_eq!(code, ExitCode::SUCCESS);
        let json = scan_json(&out);
        assert_eq!(json["total_urls"], 3, "{out}");
        assert_eq!(json["new_urls"], 1);
        assert_eq!(json["updated_urls"], 1);

        let refs = read_refs(&out_file);
        let bare = refs
            .references
            .iter()
            .find(|r| r.url.ends_with("/bare"))
            .unwrap();
        assert_eq!(bare.title, "Bare page", "the title replaces the bare URL");
        assert_eq!(bare.cited_in.len(), 2);

        // --no-merge starts over
        let fresh = [
            "scan",
            more.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
            "--no-merge",
        ];
        let (_, out, _) = ref_cmd(&fresh).await;
        assert_eq!(scan_json(&out)["total_urls"], 2, "{out}");
    }

    #[tokio::test]
    async fn scanning_the_same_file_again_changes_nothing() {
        let dir = project();
        let readme = dir.path().join("README.md");
        let out_file = dir.path().join("references.yaml");
        let line = [
            "scan",
            readme.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
        ];
        ref_cmd(&line).await;
        let before = std::fs::read_to_string(&out_file).unwrap();
        let (code, out, _) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::SUCCESS);
        let json = scan_json(&out);
        assert_eq!(
            (json["new_urls"].as_u64(), json["updated_urls"].as_u64()),
            (Some(0), Some(0)),
            "{out}"
        );
        assert_eq!(std::fs::read_to_string(&out_file).unwrap(), before);
    }

    #[tokio::test]
    async fn a_title_found_later_replaces_a_bare_url_within_one_scan() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("a.md"), dir.path().join("b.md"));
        std::fs::write(&first, "https://example.com/page\n").unwrap();
        std::fs::write(&second, "[The page](https://example.com/page)\n").unwrap();
        let out_file = dir.path().join("references.yaml");
        let line = [
            "scan",
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
        ];
        let (code, _, _) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::SUCCESS);
        let refs = read_refs(&out_file);
        assert_eq!(refs.references.len(), 1);
        assert_eq!(refs.references[0].title, "The page");
        assert_eq!(refs.references[0].cited_in.len(), 2);
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_read_or_an_output_that_cannot_be_written_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // Not UTF-8, so it cannot be read as text
        let binary = dir.path().join("binary.md");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        let out_file = dir.path().join("references.yaml");
        let line = [
            "scan",
            binary.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
        ];
        let (code, _, err) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("Failed to read"), "{err}");

        // The folder for the output does not exist
        let good = dir.path().join("good.md");
        std::fs::write(&good, "https://example.com/a").unwrap();
        let nowhere = dir.path().join("no-such-folder/references.yaml");
        let line = [
            "scan",
            good.to_str().unwrap(),
            "-o",
            nowhere.to_str().unwrap(),
        ];
        let (code, _, err) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("Failed to write"), "{err}");
    }

    #[tokio::test]
    async fn nothing_to_scan_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let out_file = dir.path().join("references.yaml");
        let missing = dir.path().join("missing.md");
        let line = [
            "scan",
            missing.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
        ];
        let (code, out, _) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(scan_json(&out)["scanned_files"], 0, "{out}");
        assert!(!out_file.exists());
    }

    #[tokio::test]
    async fn an_unreadable_file_or_a_broken_references_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let folder_named_md = dir.path().join("looks-like.md");
        std::fs::create_dir(&folder_named_md).unwrap();
        // A glob matches only files, so a folder is skipped, not read
        let glob = dir.path().join("*.md");
        let out_file = dir.path().join("references.yaml");
        let line = [
            "scan",
            glob.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
        ];
        let (code, _, _) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::SUCCESS);

        // An existing output that is not a references file stops the merge
        std::fs::write(dir.path().join("a.md"), "https://example.com/a").unwrap();
        std::fs::write(&out_file, "meta: [not, a, mapping").unwrap();
        let a = dir.path().join("a.md");
        let line = [
            "scan",
            a.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
        ];
        let (code, _, _) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::from(2));
    }

    #[test]
    fn categories_come_from_the_path() {
        let cases = [
            ("docs/a.md", "documentation"),
            ("/x/research/a.md", "research"),
            ("adr/ADR-1.md", "architecture"),
            ("/x/api/a.md", "api"),
            ("/x/tests/a.md", "testing"),
            ("README.md", "readme"),
            ("CHANGELOG.md", "changelog"),
            ("LICENSE.md", "legal"),
            ("notes.md", "general"),
        ];
        for (path, category) in cases {
            assert!(
                infer_categories(path).contains(&category.to_string()),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn the_command_refuses_to_overwrite_other_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (notes, main) = (dir.path().join("notes.md"), dir.path().join("main.rs"));
        std::fs::write(&notes, "See https://example.com/a").expect("write");
        std::fs::write(&main, "fn main() {}").expect("write");
        let (notes, main_path) = (notes.to_str().unwrap(), main.to_str().unwrap());
        let line = ["scan", notes, "--output", main_path, "--no-merge"];
        let (code, _, _) = crate::test_server::ref_cmd(&line).await;
        assert_eq!(code, std::process::ExitCode::from(2));
        assert_eq!(
            std::fs::read_to_string(&main).expect("read"),
            "fn main() {}"
        );
    }

    #[test]
    fn test_extract_markdown_urls() {
        let content = r"
# Test Document

Check out [Example Site](https://example.com) for more info.
Also see [GitHub](https://github.com/foo/bar).

Bare URL: https://bare.example.org/path

Another [link](https://another.com/page?q=1).
        ";

        let found = extract_markdown_urls(content, Path::new("test.md"));

        assert_eq!(found.len(), 4);

        // Check markdown links have titles
        let example = found
            .iter()
            .find(|f| f.url == "https://example.com")
            .unwrap();
        assert_eq!(example.title, Some("Example Site".to_string()));

        // Check bare URL has no title
        let bare = found
            .iter()
            .find(|f| f.url == "https://bare.example.org/path")
            .unwrap();
        assert_eq!(bare.title, None);
    }

    #[test]
    fn test_infer_categories() {
        assert!(infer_categories("docs/adr/ADR-001.md").contains(&"architecture".to_string()));
        assert!(infer_categories("research/topic.md").contains(&"research".to_string()));
        assert!(infer_categories("README.md").contains(&"readme".to_string()));
        assert!(infer_categories("random.md").contains(&"general".to_string()));
    }
}
