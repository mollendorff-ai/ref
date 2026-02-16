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

#[derive(Args)]
pub struct ScanArgs {
    /// Files or glob patterns to scan
    #[arg(required = true)]
    pub files: Vec<PathBuf>,

    /// Output file (default: references.yaml)
    #[arg(short, long, default_value = "references.yaml")]
    pub output: PathBuf,

    /// Merge with existing file instead of overwriting
    #[arg(long, default_value = "true")]
    pub merge: bool,
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
pub async fn run_scan(args: ScanArgs) -> Result<()> {
    // Expand file patterns and collect all files
    let files = expand_files(&args.files).await?;

    if files.is_empty() {
        let error = serde_json::json!({
            "error": "no_files",
            "message": "No files found matching patterns"
        });
        println!("{}", serde_json::to_string(&error)?);
        return Ok(());
    }

    // Extract URLs from all files
    let mut all_urls: Vec<FoundUrl> = Vec::new();
    for file in &files {
        let content = tokio::fs::read_to_string(file)
            .await
            .with_context(|| format!("Failed to read {}", file.display()))?;

        let found = extract_markdown_urls(&content, file);
        all_urls.extend(found);
    }

    // Dedupe and merge by URL
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

        // Add source file to cited_in if not already present
        if !entry.cited_in.contains(&found.source_file) {
            entry.cited_in.push(found.source_file.clone());
        }

        // Update title if we found a better one (non-URL)
        if let Some(title) = &found.title {
            if entry.title == entry.url && title != &entry.url {
                entry.title.clone_from(title);
            }
        }
    }

    // Load existing file if merging
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

    // Build a map of existing URLs for quick lookup
    let mut existing_urls: HashMap<String, usize> = HashMap::new();
    for (i, r) in refs_file.references.iter().enumerate() {
        existing_urls.insert(r.url.clone(), i);
    }

    // Merge new URLs
    let mut new_count = 0;
    let mut updated_count = 0;

    for (url, new_ref) in url_map {
        if let Some(&idx) = existing_urls.get(&url) {
            // Update existing: merge cited_in
            let existing = &mut refs_file.references[idx];
            for cited in &new_ref.cited_in {
                if !existing.cited_in.contains(cited) {
                    existing.cited_in.push(cited.clone());
                    updated_count += 1;
                }
            }
            // Update title if existing is just URL
            if existing.title == existing.url && new_ref.title != new_ref.url {
                existing.title = new_ref.title;
            }
        } else {
            // Add new reference
            refs_file.references.push(new_ref);
            new_count += 1;
        }
    }

    // Update meta
    refs_file.meta.total_links = refs_file.references.len();

    // Sort references by URL for consistency
    refs_file.references.sort_by(|a, b| a.url.cmp(&b.url));

    // Write file
    let yaml = serde_yaml::to_string(&refs_file)?;
    tokio::fs::write(&args.output, yaml).await?;

    // Output JSON result
    let output = ScanOutput {
        file: args.output.display().to_string(),
        scanned_files: files.len(),
        total_urls: refs_file.references.len(),
        new_urls: new_count,
        updated_urls: updated_count,
    };
    println!("{}", serde_json::to_string(&output)?);

    Ok(())
}

/// Expand file patterns to actual file paths
async fn expand_files(patterns: &[PathBuf]) -> Result<Vec<PathBuf>> {
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
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext == "md" || ext == "markdown" {
                            files.push(path);
                        }
                    }
                }
            }
        }
    }

    Ok(files)
}

/// Extract URLs from markdown content
fn extract_markdown_urls(content: &str, source_file: &Path) -> Vec<FoundUrl> {
    let mut found = Vec::new();
    let source = source_file.to_string_lossy().to_string();

    // Match markdown links: [title](url)
    let md_link_re = Regex::new(r"\[([^\]]+)\]\((https?://[^\s\)]+)\)").unwrap();
    for cap in md_link_re.captures_iter(content) {
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
    let bare_url_re = Regex::new(r#"https?://[^\s\)>\]"'`]+"#).unwrap();
    for mat in bare_url_re.find_iter(content) {
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

    #[test]
    fn test_extract_markdown_urls() {
        let content = r#"
# Test Document

Check out [Example Site](https://example.com) for more info.
Also see [GitHub](https://github.com/foo/bar).

Bare URL: https://bare.example.org/path

Another [link](https://another.com/page?q=1).
        "#;

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
