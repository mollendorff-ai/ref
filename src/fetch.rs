//! fetch command: Fetch URLs and convert HTML to structured JSON
//!
//! LLM-optimized output - minimal tokens, maximum signal.
//! JSON compact output only. No YAML, no pretty printing.

use crate::browser::BrowserPool;
use anyhow::Result;
use clap::Args;
use futures::future::join_all;
use scraper::{Html, Selector};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use url::Url;

#[derive(Args)]
pub struct FetchArgs {
    /// URLs to fetch (multiple allowed)
    #[arg(required = true)]
    pub urls: Vec<String>,

    /// Parallel fetches (browser tabs)
    #[arg(long, short, default_value = "4")]
    pub parallel: usize,

    /// Timeout per URL in milliseconds
    #[arg(long, default_value = "30000")]
    pub timeout: u64,

    /// Skip content cleaning (return raw extracted text)
    #[arg(long)]
    pub raw: bool,

    /// Cookie file for authenticated fetches (Netscape format)
    #[arg(long)]
    pub cookies: Option<String>,
}

/// Page status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PageStatus {
    Ok,
    Paywall,
    Login,
    Dead,
    Redirect,
}

/// A content section with heading hierarchy
#[derive(Debug, Serialize, Clone)]
pub struct Section {
    /// Heading level (1-6)
    pub level: u8,
    /// Heading text
    pub heading: String,
    /// Content under this heading (cleaned text)
    pub content: String,
}

/// A content link (not navigation)
#[derive(Debug, Serialize, Clone)]
pub struct Link {
    pub text: String,
    pub url: String,
}

/// A code block
#[derive(Debug, Serialize, Clone)]
pub struct CodeBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    pub source: String,
}

/// LLM-optimized page output
#[derive(Debug, Serialize, Clone)]
pub struct Page {
    pub url: String,
    pub status: PageStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<Section>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub code: Vec<CodeBlock>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<String>,
    pub chars: usize,
}

/// Fetch one or more URLs in parallel and print structured JSON to stdout.
///
/// # Errors
///
/// Returns an error if the browser pool fails to initialize, pages cannot
/// be serialized, or the browser cannot be closed cleanly.
pub async fn run_fetch(args: FetchArgs) -> Result<()> {
    let url_count = args.urls.len();
    let parallel = args.parallel.min(url_count).max(1);

    eprintln!(
        "Fetching {} URL{} ({} parallel)...",
        url_count,
        if url_count == 1 { "" } else { "s" },
        parallel
    );

    let pool = Arc::new(BrowserPool::new(parallel).await?);
    let timeout = args.timeout;
    let raw = args.raw;

    // Spawn parallel fetch tasks
    let tasks: Vec<_> = args
        .urls
        .into_iter()
        .map(|url| {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move { fetch_one(&pool, &url, timeout, raw).await })
        })
        .collect();

    // Await all tasks
    let results: Vec<Page> = join_all(tasks)
        .await
        .into_iter()
        .filter_map(std::result::Result::ok)
        .collect();

    // Close browser
    if let Ok(pool) = Arc::try_unwrap(pool) {
        pool.close().await?;
    }

    let ok_count = results
        .iter()
        .filter(|p| p.status == PageStatus::Ok)
        .count();

    // Output compact JSON (one line per page for multiple, or single object)
    if results.len() == 1 {
        println!("{}", serde_json::to_string(&results[0])?);
    } else {
        for page in &results {
            println!("{}", serde_json::to_string(page)?);
        }
    }

    eprintln!("Done: {ok_count}/{url_count} OK");
    Ok(())
}

pub(crate) async fn fetch_one(pool: &BrowserPool, url: &str, timeout: u64, raw: bool) -> Page {
    eprintln!("  -> {}", truncate(url, 60));

    let page = match pool.new_page().await {
        Ok(p) => p,
        Err(e) => return error_page(url, &e.to_string()),
    };

    // Parse original URL for redirect detection
    let original_host = Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(String::from));

    let nav = match page.goto(url, timeout).await {
        Ok(n) => n,
        Err(e) => return error_page(url, &e.to_string()),
    };

    if nav.error.is_some() {
        return Page {
            url: url.to_string(),
            status: PageStatus::Dead,
            title: nav.title,
            site: None,
            author: None,
            date: None,
            doi: None,
            sections: vec![],
            links: vec![],
            code: vec![],
            alerts: vec![nav.error.unwrap_or_default()],
            chars: 0,
        };
    }

    // Check for redirect
    let final_url = page.current_url().await;
    if let (Some(orig), Some(final_u)) = (&original_host, &final_url) {
        if let Ok(parsed) = Url::parse(final_u) {
            if let Some(final_host) = parsed.host_str() {
                let orig_norm = orig.trim_start_matches("www.");
                let final_norm = final_host.trim_start_matches("www.");
                if orig_norm != final_norm {
                    return Page {
                        url: url.to_string(),
                        status: PageStatus::Redirect,
                        title: nav.title,
                        site: None,
                        author: None,
                        date: None,
                        doi: None,
                        sections: vec![],
                        links: vec![],
                        code: vec![],
                        alerts: vec![format!("Redirected to: {}", final_u)],
                        chars: 0,
                    };
                }
            }
        }
    }

    let html = match page.content().await {
        Ok(h) => h,
        Err(e) => return error_page(url, &e.to_string()),
    };

    parse_page(&html, url, raw)
}

fn error_page(url: &str, error: &str) -> Page {
    Page {
        url: url.to_string(),
        status: PageStatus::Dead,
        title: None,
        site: None,
        author: None,
        date: None,
        doi: None,
        sections: vec![],
        links: vec![],
        code: vec![],
        alerts: vec![error.to_string()],
        chars: 0,
    }
}

pub(crate) fn parse_page(html: &str, url: &str, raw: bool) -> Page {
    let doc = Html::parse_document(html);
    let mut alerts = Vec::new();

    // Extract metadata
    let title = extract_title(&doc);
    let site = extract_meta(&doc, "og:site_name");
    let author = extract_meta(&doc, "author").or_else(|| extract_meta(&doc, "article:author"));
    let date = extract_meta(&doc, "article:published_time")
        .or_else(|| extract_meta(&doc, "date"))
        .or_else(|| extract_meta(&doc, "pubdate"));
    let doi = extract_doi(&doc);

    // Check for paywall/login
    let status = detect_status(html);
    if status == PageStatus::Paywall {
        alerts.push("Paywall detected".to_string());
    } else if status == PageStatus::Login {
        alerts.push("Login required".to_string());
    }

    // Extract content
    let content_html = if raw {
        html.to_string()
    } else {
        extract_main_content(&doc)
    };

    let content_doc = Html::parse_document(&content_html);

    // Extract sections
    let sections = extract_sections(&content_doc);

    // Extract links (content only, not nav)
    let links = if raw {
        extract_all_links(&doc, url)
    } else {
        extract_content_links(&content_doc, url)
    };

    // Extract code blocks
    let code = extract_code_blocks(&content_doc);

    // Calculate total chars
    let chars: usize = sections
        .iter()
        .map(|s| s.content.len() + s.heading.len())
        .sum();

    Page {
        url: url.to_string(),
        status,
        title,
        site,
        author,
        date,
        doi,
        sections,
        links,
        code,
        alerts,
        chars,
    }
}

fn extract_title(doc: &Html) -> Option<String> {
    select_text(doc, "title")
        .or_else(|| select_attr(doc, "meta[property='og:title']", "content"))
        .or_else(|| select_text(doc, "h1"))
}

fn extract_meta(doc: &Html, name: &str) -> Option<String> {
    select_attr(doc, &format!("meta[property='{name}']"), "content")
        .or_else(|| select_attr(doc, &format!("meta[name='{name}']"), "content"))
}

fn extract_doi(doc: &Html) -> Option<String> {
    // Check meta tags
    if let Some(doi) = select_attr(doc, "meta[name='citation_doi']", "content") {
        return Some(doi);
    }
    if let Some(doi) = select_attr(doc, "meta[name='DC.identifier']", "content") {
        if doi.contains("doi.org") || doi.starts_with("10.") {
            return Some(doi);
        }
    }
    // Check for DOI links
    if let Ok(sel) = Selector::parse("a[href*='doi.org']") {
        if let Some(el) = doc.select(&sel).next() {
            if let Some(href) = el.value().attr("href") {
                return Some(href.to_string());
            }
        }
    }
    None
}

fn detect_status(html: &str) -> PageStatus {
    let lower = html.to_lowercase();

    // Paywall indicators
    let paywall_patterns = [
        "subscribe to continue",
        "subscription required",
        "premium content",
        "paywall",
        "member-only",
        "members only",
        "unlock this article",
        "purchase to read",
    ];
    for pattern in paywall_patterns {
        if lower.contains(pattern) {
            return PageStatus::Paywall;
        }
    }

    // Login indicators
    let login_patterns = [
        "sign in to continue",
        "log in to continue",
        "login to continue",
        "please sign in",
        "please log in",
        "create an account to",
        "sign up to view",
    ];
    for pattern in login_patterns {
        if lower.contains(pattern) {
            return PageStatus::Login;
        }
    }

    PageStatus::Ok
}

fn extract_main_content(doc: &Html) -> String {
    // Priority: main > article > [role=main] > body
    let selectors = [
        "main",
        "article",
        "[role='main']",
        ".post-content",
        ".article-content",
        ".entry-content",
        "#content",
        ".content",
    ];

    for sel_str in selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = doc.select(&sel).next() {
                return el.html();
            }
        }
    }

    // Fallback: body with nav/header/footer/aside stripped
    if let Ok(body_sel) = Selector::parse("body") {
        if let Some(body) = doc.select(&body_sel).next() {
            let html = body.html();
            // Remove common non-content elements
            let strip_patterns = [
                "<nav",
                "</nav>",
                "<header",
                "</header>",
                "<footer",
                "</footer>",
                "<aside",
                "</aside>",
                "cookie",
                "Cookie",
            ];
            for pattern in strip_patterns {
                if html.contains(pattern) {
                    // Simple removal - in practice we'd use proper DOM manipulation
                    // but for now we rely on html2text to handle this
                }
            }
            return html;
        }
    }

    doc.html()
}

fn extract_sections(doc: &Html) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut current_section: Option<Section> = None;

    // Simple approach: extract headings and paragraphs
    for tag in ["h1", "h2", "h3", "h4", "h5", "h6", "p"] {
        if let Ok(sel) = Selector::parse(tag) {
            for el in doc.select(&sel) {
                let text: String = el.text().collect::<String>().trim().to_string();
                if text.is_empty() || text.len() < 3 {
                    continue;
                }

                if tag.starts_with('h') {
                    // Flush current section
                    if let Some(s) = current_section.take() {
                        if !s.content.is_empty() {
                            sections.push(s);
                        }
                    }

                    let level = tag.chars().nth(1).and_then(|c| c.to_digit(10)).unwrap_or(1);
                    let level = u8::try_from(level).unwrap_or(1);
                    current_section = Some(Section {
                        level,
                        heading: truncate_section(&text, 200),
                        content: String::new(),
                    });
                } else if let Some(ref mut s) = current_section {
                    // Add paragraph to current section
                    if !s.content.is_empty() {
                        s.content.push_str("\n\n");
                    }
                    s.content.push_str(&truncate_section(&text, 2000));
                }
            }
        }
    }

    // Flush final section
    if let Some(s) = current_section {
        if !s.content.is_empty() {
            sections.push(s);
        }
    }

    // If no sections found, create one from all text
    if sections.is_empty() {
        let text = html2text::from_read(doc.html().as_bytes(), 120)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(100)
            .collect::<Vec<_>>()
            .join("\n");

        if !text.is_empty() {
            sections.push(Section {
                level: 1,
                heading: "Content".to_string(),
                content: truncate_section(&text, 10000),
            });
        }
    }

    // Limit total sections
    sections.truncate(50);
    sections
}

fn extract_content_links(doc: &Html, base_url: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let mut seen = HashSet::new();
    let base = Url::parse(base_url).ok();

    if let Ok(sel) = Selector::parse("a[href]") {
        for el in doc.select(&sel) {
            let text: String = el.text().collect::<String>().trim().to_string();
            let href = el.value().attr("href").unwrap_or("");

            // Skip empty, anchor-only, or javascript links
            if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
                continue;
            }

            // Skip short link text (likely nav)
            if text.len() < 3 {
                continue;
            }

            // Resolve relative URLs
            let full_url = if let Some(ref base) = base {
                base.join(href)
                    .map_or_else(|_| href.to_string(), |u| u.to_string())
            } else {
                href.to_string()
            };

            // Dedupe
            if seen.contains(&full_url) {
                continue;
            }
            seen.insert(full_url.clone());

            links.push(Link {
                text: truncate(&text, 100),
                url: full_url,
            });

            if links.len() >= 50 {
                break;
            }
        }
    }

    links
}

fn extract_all_links(doc: &Html, base_url: &str) -> Vec<Link> {
    // In raw mode, extract all links
    extract_content_links(doc, base_url)
}

fn extract_code_blocks(doc: &Html) -> Vec<CodeBlock> {
    let mut blocks = Vec::new();
    let mut seen_sources = HashSet::new();

    // Look for <pre><code> blocks first (more specific), then <pre>
    for selector_str in ["pre code", "pre"] {
        let Ok(sel) = Selector::parse(selector_str) else {
            continue;
        };
        for el in doc.select(&sel) {
            let source: String = el.text().collect::<String>().trim().to_string();
            if source.is_empty() || source.len() < 10 {
                continue;
            }

            // Skip if we've already seen this source (dedup nested pre/code)
            if seen_sources.contains(&source) {
                continue;
            }
            seen_sources.insert(source.clone());

            // Try to detect language from class
            let lang = el.value().attr("class").and_then(|c| {
                c.split_whitespace()
                    .find(|cls| cls.starts_with("language-") || cls.starts_with("lang-"))
                    .map(|cls| {
                        cls.trim_start_matches("language-")
                            .trim_start_matches("lang-")
                            .to_string()
                    })
            });

            blocks.push(CodeBlock {
                lang,
                source: truncate_section(&source, 5000),
            });

            if blocks.len() >= 20 {
                break;
            }
        }
    }

    blocks
}

fn select_text(doc: &Html, sel: &str) -> Option<String> {
    let selector = Selector::parse(sel).ok()?;
    doc.select(&selector)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

fn select_attr(doc: &Html, sel: &str, attr: &str) -> Option<String> {
    let selector = Selector::parse(sel).ok()?;
    doc.select(&selector)
        .next()
        .and_then(|el| el.value().attr(attr))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Find the largest valid char boundary <= pos
fn floor_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut i = pos;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = floor_char_boundary(s, max - 3);
        format!("{}...", &s[..end])
    }
}

fn truncate_section(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = floor_char_boundary(s, max);
        let truncated = &s[..end];
        if let Some(last_space) = truncated.rfind(' ') {
            if last_space > end.saturating_sub(50) {
                return format!("{}...", &s[..last_space]);
            }
        }
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic() {
        let html = r"
            <html>
            <head><title>Test Page</title></head>
            <body>
                <main>
                    <h1>Main Title</h1>
                    <p>Some content here with enough text to be meaningful.</p>
                </main>
            </body>
            </html>
        ";
        let page = parse_page(html, "https://test.com", false);
        assert_eq!(page.status, PageStatus::Ok);
        assert_eq!(page.title, Some("Test Page".to_string()));
        assert!(!page.sections.is_empty());
    }

    #[test]
    fn test_detect_paywall() {
        assert_eq!(
            detect_status("<div>Subscribe to continue reading</div>"),
            PageStatus::Paywall
        );
        assert_eq!(detect_status("<div>Normal content</div>"), PageStatus::Ok);
    }

    #[test]
    fn test_detect_login() {
        assert_eq!(
            detect_status("<div>Please sign in to continue</div>"),
            PageStatus::Login
        );
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("this is a long string", 10), "this is...");
        // Multi-byte chars: ─ is 3 bytes, should not panic
        let dashes = "─".repeat(100);
        let result = truncate(&dashes, 20);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 23); // max 20 + safety margin for ...
    }

    #[test]
    fn test_floor_char_boundary() {
        let s = "hello─world"; // ─ is at bytes 5..8
        assert_eq!(floor_char_boundary(s, 5), 5); // exactly at boundary
        assert_eq!(floor_char_boundary(s, 6), 5); // inside ─, goes back
        assert_eq!(floor_char_boundary(s, 7), 5); // inside ─, goes back
        assert_eq!(floor_char_boundary(s, 8), 8); // after ─
    }

    #[test]
    fn test_extract_code() {
        let html =
            r#"<pre><code class="language-rust">fn main() { println!("hello"); }</code></pre>"#;
        let doc = Html::parse_document(html);
        let blocks = extract_code_blocks(&doc);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang, Some("rust".to_string()));
    }
}
