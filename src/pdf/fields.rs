//! Document fields found in PDF text: title, author, date, DOI, links, code.

use crate::fetch::{CodeBlock, Link};
use crate::text::truncate;
use regex::Regex;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

/// See [`extract_author`]. Names use Latin letters (ASCII, Latin-1 and
/// Latin Extended-A, so "Zoë Łukasiewicz" matches): Unicode classes such as
/// `\p{L}` take about 30 ms to compile, paid on every PDF.
static AUTHOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^[ \t]*(?:(?i:authors?|written by)[ \t]*:?[ \t]+([A-ZÀ-ÖØ-ÞĀ-ſ][^\n]{0,150})|(?i:by)[ \t]+([A-ZÀ-ÖØ-ÞĀ-ſ][A-Za-zÀ-ÖØ-öø-ÿĀ-ſ.' -]{1,60}(?:,[ \t]*[A-ZÀ-ÖØ-ÞĀ-ſ][A-Za-zÀ-ÖØ-öø-ÿĀ-ſ.' -]{1,60})*))[ \t]*$",
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
    fn the_title_is_the_first_line_or_else_the_file_name() {
        let path = Path::new("/docs/annual-report.pdf");
        assert_eq!(
            extract_title("\n  Annual Report 2026  \nbody", path).as_deref(),
            Some("Annual Report 2026")
        );
        // Too short or too long to be a title
        assert_eq!(
            extract_title("ab\nbody", path).as_deref(),
            Some("annual-report")
        );
        assert_eq!(
            extract_title(&"x".repeat(300), path).as_deref(),
            Some("annual-report")
        );
        assert_eq!(extract_title("", path).as_deref(), Some("annual-report"));
        // No file name either
        assert_eq!(extract_title("", Path::new("")), None);
    }

    #[test]
    fn dates_dois_and_authors_are_found_in_the_text() {
        assert_eq!(
            extract_date("Date: 2026-11-06").as_deref(),
            Some("2026-11-06")
        );
        assert_eq!(
            extract_date("Published November 6, 2026").as_deref(),
            Some("November 6, 2026")
        );
        assert_eq!(extract_date("no date here"), None);
        assert_eq!(
            extract_doi("see doi: 10.1234/abc.def for more").as_deref(),
            Some("10.1234/abc.def")
        );
        assert_eq!(extract_doi("nothing"), None);
        assert_eq!(
            extract_author("Author: Jane Doe\nbody").as_deref(),
            Some("Jane Doe")
        );
        assert_eq!(extract_author("sold by region, in thousands"), None);
    }

    #[test]
    fn links_are_deduplicated_cleaned_and_limited_to_fifty() {
        let text = "See https://a.example/x, and https://a.example/x. Also (https://b.example/y).";
        let urls: Vec<_> = extract_links(text).into_iter().map(|l| l.url).collect();
        assert_eq!(urls, ["https://a.example/x", "https://b.example/y"]);
        let many = (0..80).fold(String::new(), |all, i| {
            all + &format!("https://c.example/{i} ")
        });
        assert_eq!(extract_links(&many).len(), 50);
    }

    #[test]
    fn code_is_recognised_by_indentation_and_keywords_and_named() {
        let rust = "Text before\n    fn add(a: i32) -> i32 {\n        a + 1\n    }\nText after";
        let blocks = extract_code(rust);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang.as_deref(), Some("rust"));

        // A block that ends the text is kept too
        let python = "intro\n    def add(a, b):\n        return a + b\n    print(add(1, 2))";
        assert_eq!(extract_code(python)[0].lang.as_deref(), Some("python"));

        // Too short (under 3 lines, or under 20 characters) is not a block
        assert!(extract_code("    a\n    b\ntext").is_empty());
        assert!(extract_code("text\n    a\n    b\n    c\ntext").is_empty());
        assert!(extract_code("    a\n    b\n    c").is_empty());
    }

    #[test]
    fn the_language_comes_from_what_the_code_uses() {
        let cases = [
            ("fn main() -> () {}", Some("rust")),
            ("def run():\n    pass", Some("python")),
            ("function go() {}", Some("javascript")),
            ("const x = 1;", Some("javascript")),
            ("let y = 2;", Some("javascript")),
            ("public class A {}", Some("java")),
            ("private int n;", Some("java")),
            ("#include <stdio.h>", Some("c")),
            ("SELECT 1;", None),
        ];
        for (code, language) in cases {
            assert_eq!(detect_language(code).as_deref(), language, "{code}");
        }
    }

    #[test]
    fn at_most_ten_code_blocks_are_returned() {
        let block = "    fn go() -> u8 {\n        1\n    }\ntext\n";
        let blocks = extract_code(&block.repeat(15));
        assert_eq!(blocks.len(), 10);
    }

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
        // Accented Latin names, labelled or not
        assert_eq!(
            extract_author("By Zoë Łukasiewicz, José Ávila\n"),
            Some("Zoë Łukasiewicz, José Ávila".to_string())
        );
        assert_eq!(
            extract_author("Author: Élodie Côté\n"),
            Some("Élodie Côté".to_string())
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
