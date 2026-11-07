//! Document fields found in PDF text: title, author, date, DOI, links, code.

use crate::fetch::{CodeBlock, Link};
use crate::text::truncate;
use regex::Regex;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

/// See [`extract_author`]
static AUTHOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^[ \t]*(?:(?i:authors?|written by)[ \t]*:?[ \t]+(\p{Lu}[^\n]{0,150})|(?i:by)[ \t]+(\p{Lu}[\p{L}.' -]{1,60}(?:,[ \t]*\p{Lu}[\p{L}.' -]{1,60})*))[ \t]*$",
    )
    .expect("constant regex")
});

/// "Date: 2026-11-06", "Published November 6, 2026"
static DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:date|published|updated)[:\s]+(\d{4}[-/]\d{2}[-/]\d{2}|\w+\s+\d{1,2},?\s+\d{4})",
    )
    .expect("constant regex")
});

/// "doi: 10.1000/xyz" or a doi.org link
static DOI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:doi[:\s]+|https?://doi\.org/)(10\.\d{4,}/[^\s\)]+)")
        .expect("constant regex")
});

/// http(s) URLs in the text
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s\)>\]"']+"#).expect("constant regex"));

pub(super) fn extract_title(text: &str, path: &Path) -> Option<String> {
    let first_line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string());

    if let Some(ref line) = first_line {
        if line.len() < 200 && line.len() > 3 {
            return Some(truncate(line, 200));
        }
    }

    path.file_stem()
        .and_then(|s| s.to_str())
        .map(std::string::ToString::to_string)
}

pub(super) fn extract_author(text: &str) -> Option<String> {
    // A labelled line ("Author: Jane Doe", "Written by Jane Doe"), or a line
    // that is only "By" and a list of names ("By Jane Doe, Ana Lima").
    // "by region, in thousands" inside a sentence is neither. Only the
    // keywords ignore case: names must start with a capital.
    AUTHOR_RE
        .captures(text)
        .and_then(|c| c.get(1).or_else(|| c.get(2)))
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| s.len() < 200)
}

pub(super) fn extract_date(text: &str) -> Option<String> {
    DATE_RE.captures(text).map(|c| c[1].trim().to_string())
}

pub(super) fn extract_doi(text: &str) -> Option<String> {
    DOI_RE.captures(text).map(|c| c[1].trim().to_string())
}

pub(super) fn extract_links(text: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let mut seen = HashSet::new();

    for mat in URL_RE.find_iter(text) {
        let url = mat
            .as_str()
            .trim_end_matches([',', '.', ')', ']', ';', ':']);
        if !seen.contains(url) {
            seen.insert(url.to_string());
            links.push(Link {
                text: truncate(url, 100),
                url: url.to_string(),
            });
        }
        if links.len() >= 50 {
            break;
        }
    }

    links
}

pub(super) fn extract_code(text: &str) -> Vec<CodeBlock> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = text.lines().collect();

    let mut in_code_block = false;
    let mut code_lines = Vec::new();

    for line in lines {
        let is_code_line = line.starts_with("    ")
            || line.starts_with('\t')
            || (line.contains("def ")
                || line.contains("fn ")
                || line.contains("function ")
                || line.contains("class ")
                || line.contains("import ")
                || line.contains("package ")
                || line.contains("//")
                || line.contains("/*")
                || line.contains("#include"));

        if is_code_line {
            in_code_block = true;
            code_lines.push(line.to_string());
        } else if in_code_block {
            if code_lines.len() >= 3 {
                let source = code_lines.join("\n");
                if source.len() >= 20 {
                    blocks.push(CodeBlock {
                        lang: detect_language(&source),
                        source: truncate(&source, 5000),
                    });
                }
            }
            code_lines.clear();
            in_code_block = false;
        }

        if blocks.len() >= 10 {
            break;
        }
    }

    if code_lines.len() >= 3 {
        let source = code_lines.join("\n");
        if source.len() >= 20 {
            blocks.push(CodeBlock {
                lang: detect_language(&source),
                source: truncate(&source, 5000),
            });
        }
    }

    blocks
}

fn detect_language(code: &str) -> Option<String> {
    if code.contains("fn ") && code.contains("->") {
        Some("rust".to_string())
    } else if code.contains("def ") && code.contains(':') {
        Some("python".to_string())
    } else if code.contains("function ") || code.contains("const ") || code.contains("let ") {
        Some("javascript".to_string())
    } else if code.contains("public class") || code.contains("private ") {
        Some("java".to_string())
    } else if code.contains("#include") {
        Some("c".to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_lines_are_labels_or_names_only() {
        assert_eq!(
            extract_author("Author: Jane Doe\nText"),
            Some("Jane Doe".to_string())
        );
        assert_eq!(
            extract_author("Title\nBy Jane Doe, Ana Lima\n"),
            Some("Jane Doe, Ana Lima".to_string())
        );
        assert_eq!(
            extract_author("Revenue by region, in thousands of dollars:"),
            None
        );
        assert_eq!(
            extract_author("by Michael Everson at the “Alphabets” (http://x) site. If"),
            None
        );
        assert_eq!(
            extract_author("By the end of the quarter, sales grew."),
            None
        );
    }

    #[test]
    fn test_extract_doi() {
        let text = "This paper (DOI: 10.1234/abc.123) presents...";
        assert_eq!(extract_doi(text), Some("10.1234/abc.123".to_string()));
    }

    #[test]
    fn test_extract_links() {
        let text = "See https://example.com and https://foo.bar/path for details.";
        let links = extract_links(text);
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn test_detect_language() {
        assert_eq!(
            detect_language("fn main() -> i32 { 42 }"),
            Some("rust".to_string())
        );
        assert_eq!(
            detect_language("def foo(): pass"),
            Some("python".to_string())
        );
    }
}
