//! fetch command: Fetch URLs and convert HTML to structured JSON
//!
//! LLM-optimized output - minimal tokens, maximum signal.
//! JSON compact output only. No YAML, no pretty printing.

use crate::access::{self, Access};
use crate::browser::BrowserPool;
pub use crate::content::Section;
use crate::content::{self, DEFAULT_MAX_SECTIONS};
use crate::text::{cut_at_word, truncate};
use anyhow::{anyhow, Context, Result};
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

    /// Parallel fetches (browser tabs, 1-20)
    #[arg(long, short, default_value = "4", value_parser = clap::value_parser!(u8).range(1..=20))]
    pub parallel: u8,

    /// Timeout per URL in milliseconds
    #[arg(long, default_value = "30000")]
    pub timeout: u64,

    /// Skip content cleaning (return raw extracted text)
    #[arg(long)]
    pub raw: bool,

    /// Send this exact user agent instead of Chrome's own with ref's name added.
    /// Some sites want a declared name and email, e.g. SEC EDGAR:
    /// --user-agent "Jane Doe jane@example.com"
    #[arg(long, value_name = "TEXT")]
    pub user_agent: Option<String>,

    /// Cookie file for authenticated fetches (Netscape format)
    #[arg(long)]
    pub cookies: Option<String>,

    /// CSS selector to extract a specific element (skips content heuristics)
    #[arg(long)]
    pub selector: Option<String>,

    /// Maximum characters per section; longer sections are cut and marked
    /// "truncated" (0 = no limit)
    #[arg(long, default_value_t = DEFAULT_MAX_CHARS)]
    pub max_chars: usize,

    /// Maximum number of sections per page; more are dropped and the page is
    /// marked "truncated" (0 = no limit)
    #[arg(long, default_value_t = DEFAULT_MAX_SECTIONS)]
    pub max_sections: usize,

    #[command(flatten)]
    pub profile: crate::profile::ProfileArgs,
}

/// Default per-section character limit
pub const DEFAULT_MAX_CHARS: usize = 5000;

/// Maximum links returned per page
const MAX_LINKS: usize = 50;

/// How to fetch and trim one page
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Navigation timeout in milliseconds
    pub timeout: u64,
    /// Keep page chrome and all links
    pub raw: bool,
    /// CSS selector for the element to extract
    pub selector: Option<String>,
    /// Characters per section (0 = no limit)
    pub max_chars: usize,
    /// Sections per page (0 = no limit)
    pub max_sections: usize,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout: 30_000,
            raw: false,
            selector: None,
            max_chars: DEFAULT_MAX_CHARS,
            max_sections: DEFAULT_MAX_SECTIONS,
        }
    }
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
    /// HTTP status code of the main document, when Chrome reported one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<u16>,
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
    /// True when any content was cut to the size limits (see `alerts`)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Fetch one or more URLs in parallel and print structured JSON to stdout.
///
/// Returns `true` when no page came back dead (4xx, 5xx or unreachable).
///
/// # Errors
///
/// Returns an error if the browser pool fails to initialize, pages cannot
/// be serialized, or the browser cannot be closed cleanly.
pub async fn run_fetch(args: FetchArgs, out: &mut dyn std::io::Write) -> Result<bool> {
    let url_count = args.urls.len();
    let parallel = usize::from(args.parallel).min(url_count).max(1);

    eprintln!(
        "Fetching {} URL{} ({} parallel)...",
        url_count,
        if url_count == 1 { "" } else { "s" },
        parallel
    );

    let mut pool = BrowserPool::new(parallel, args.profile.choice()?.as_ref()).await?;
    if let Some(ua) = &args.user_agent {
        pool.set_user_agent(ua)?;
    }
    if let Some(path) = &args.cookies {
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read cookie file {path}"))?;
        let cookies = crate::cookies::parse(&text).map_err(|e| anyhow!("{path}: {e}"))?;
        eprintln!("Loaded {} cookies from {path}", cookies.len());
        pool.set_cookies(&cookies);
    }
    let pool = Arc::new(pool);
    let options = Arc::new(FetchOptions {
        timeout: args.timeout,
        raw: args.raw,
        selector: args.selector,
        max_chars: args.max_chars,
        max_sections: args.max_sections,
    });

    // Spawn parallel fetch tasks
    let tasks: Vec<_> = args
        .urls
        .into_iter()
        .map(|url| {
            let pool = Arc::clone(&pool);
            let options = Arc::clone(&options);
            tokio::spawn(async move { fetch_one(&pool, &url, &options).await })
        })
        .collect();

    // Await all tasks (a task that panicked is left out, and the run fails)
    let results: Vec<Result<Page>> = join_all(tasks)
        .await
        .into_iter()
        .filter_map(std::result::Result::ok)
        .collect();

    pool.stop().await;
    // A Chrome failure ends the command with no report (ADR-005 rule 3)
    let results: Vec<Page> = results.into_iter().collect::<Result<_>>()?;

    let ok_count = results
        .iter()
        .filter(|p| p.status == PageStatus::Ok)
        .count();

    // Output compact JSON (one line per page for multiple, or single object)
    if results.len() == 1 {
        writeln!(out, "{}", serde_json::to_string(&results[0])?)?;
    } else {
        for page in &results {
            writeln!(out, "{}", serde_json::to_string(page)?)?;
        }
    }

    eprintln!("Done: {ok_count}/{url_count} OK");
    let none_dead = results.iter().all(|p| p.status != PageStatus::Dead);
    Ok(none_dead && results.len() == url_count)
}

/// Fetch one URL.
///
/// # Errors
/// Returns [`ChromeFailed`](crate::browser::ChromeFailed) when Chrome failed;
/// every failure of the page itself is in the returned [`Page`].
pub(crate) async fn fetch_one(
    pool: &BrowserPool,
    url: &str,
    options: &FetchOptions,
) -> Result<Page> {
    eprintln!("  -> {}", truncate(url, 60));
    match fetch_page(pool, url, options).await {
        Err(e) if !e.is::<crate::browser::ChromeFailed>() => Ok(error_page(url, &e.to_string())),
        fetched => fetched,
    }
}

/// [`fetch_one`] with every failure an error: Chrome's, or the page's
async fn fetch_page(pool: &BrowserPool, url: &str, options: &FetchOptions) -> Result<Page> {
    crate::guard::check_web_url(url).map_err(|e| anyhow!(e))?;

    let page = pool.new_page().await?;
    let nav = page.goto(url, options.timeout).await?;

    if let Some(error) = nav.error {
        return Ok(Page {
            status: PageStatus::Dead,
            title: nav.title,
            ..error_page(url, &error)
        });
    }

    // A PDF opens in Chrome's viewer, which has no readable HTML: extract it instead
    if nav.mime.as_deref() == Some("application/pdf") && nav.status.is_some_and(|c| c < 400) {
        drop(page);
        // Same user agent and cookies as Chrome, so a signed-in PDF downloads too
        let headers = pool.download_headers(url);
        let mut pdf = crate::pdf::extract_pdf_from_url_with(url, &headers)
            .await
            .base;
        pdf.alerts
            .insert(0, "PDF detected: extracted as with `ref pdf`".to_string());
        if pdf.status != PageStatus::Dead {
            pdf.http = nav.status;
        }
        return Ok(pdf);
    }

    // Check for redirect
    if let Some(moved_to) = redirect_target(url, page.current_url().await?) {
        return Ok(Page {
            status: PageStatus::Redirect,
            http: nav.status,
            title: nav.title,
            alerts: vec![format!("Redirected to: {moved_to}")],
            ..error_page(url, "")
        });
    }

    let html = page.content().await?;
    let mut parsed = parse_page(&html, url, options);
    apply_http_status(&mut parsed, nav.status);
    if nav.status == Some(403) && !pool.has_custom_user_agent() {
        parsed.alerts.insert(1, USER_AGENT_HINT.to_string());
    }
    Ok(parsed)
}

/// Where `url` ended up, when that is another host (`www.` does not count);
/// `None` for the same host, or when either address has no host
pub(crate) fn redirect_target(url: &str, final_url: Option<String>) -> Option<String> {
    let final_url = final_url?;
    let host = |address: &str| {
        let parsed = Url::parse(address).ok()?;
        let host = parsed.host_str()?;
        Some(host.trim_start_matches("www.").to_string())
    };
    match (host(url), host(&final_url)) {
        (Some(from), Some(to)) if from != to => Some(final_url),
        _ => None,
    }
}

/// Added after "HTTP 403" when ref used its own user agent
const USER_AGENT_HINT: &str = "The site refused the request. Some sites (SEC EDGAR, for one) \
     want a declared user agent with a name and an email: try --user-agent \"Name email\"";

/// Record the HTTP status and mark 4xx and 5xx responses as dead.
fn apply_http_status(page: &mut Page, http: Option<u16>) {
    page.http = http;
    if let Some(code) = http.filter(|c| *c >= 400) {
        page.status = PageStatus::Dead;
        page.alerts.insert(0, format!("HTTP {code}"));
    }
}

fn error_page(url: &str, error: &str) -> Page {
    Page {
        url: url.to_string(),
        status: PageStatus::Dead,
        http: None,
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
        truncated: false,
    }
}

pub(crate) fn parse_page(html: &str, url: &str, options: &FetchOptions) -> Page {
    let raw = options.raw;
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
    let content_html = if let Some(sel_str) = options.selector.as_deref() {
        let matched =
            Selector::parse(sel_str).map(|sel| doc.select(&sel).next().map(|el| el.html()));
        if let Ok(Some(html)) = matched {
            html
        } else {
            let why = if matched.is_err() {
                "is not valid CSS"
            } else {
                "matched nothing"
            };
            alerts.push(format!(
                "Selector {sel_str:?} {why}; returned the main content instead"
            ));
            content::main_content(&doc)
        }
    } else if raw {
        html.to_string()
    } else {
        content::main_content(&doc)
    };

    let content_doc = Html::parse_document(&content_html);

    // Extract sections
    let extracted =
        content::extract_sections(&content_doc, options.max_chars, options.max_sections, raw);
    let sections = extracted.sections;
    let mut truncated = extracted.truncated;

    // Extract links (content only, not nav)
    let (links, links_capped) = extract_content_links(if raw { &doc } else { &content_doc }, url);
    if links_capped {
        alerts.push(format!("Links limited to the first {MAX_LINKS}"));
    }

    // Extract code blocks
    let (code, code_truncated) = extract_code_blocks(&content_doc);
    truncated |= code_truncated;
    if truncated {
        alerts.push(truncation_alert(options));
    }

    // Calculate total chars (characters, not bytes)
    let chars: usize = sections
        .iter()
        .map(|s| s.content.chars().count() + s.heading.chars().count())
        .sum();
    if chars == 0 && status == PageStatus::Ok {
        alerts.push(
            "No readable content found; try --raw or --selector (the page may render later or block automated browsers)"
                .to_string(),
        );
    }

    Page {
        url: url.to_string(),
        status,
        http: None,
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
        truncated,
    }
}

fn truncation_alert(options: &FetchOptions) -> String {
    let mut limits = Vec::new();
    if options.max_chars > 0 {
        limits.push(format!("{} characters per section", options.max_chars));
    }
    if options.max_sections > 0 {
        limits.push(format!("{} sections", options.max_sections));
    }
    limits.push(format!(
        "{MAX_CODE_BLOCKS} code blocks of up to {MAX_CODE_CHARS} characters"
    ));
    format!(
        "Content truncated: limits are {}. Use --max-chars 0 --max-sections 0 for full text",
        limits.join(", ")
    )
}

/// Page title from `<title>`, `og:title` or the first `<h1>`
pub(crate) fn page_title(doc: &Html) -> Option<String> {
    extract_title(doc)
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
    select_attr(doc, "a[href*='doi.org']", "href")
}

/// A selector written in this file, which always parses
fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("constant selector")
}

fn detect_status(html: &str) -> PageStatus {
    match access::detect(html) {
        Access::Open => PageStatus::Ok,
        Access::Paywall => PageStatus::Paywall,
        Access::Login => PageStatus::Login,
    }
}

/// Content links, deduplicated; the flag is true when more than [`MAX_LINKS`] were found
fn extract_content_links(doc: &Html, base_url: &str) -> (Vec<Link>, bool) {
    let mut links = Vec::new();
    let mut seen = HashSet::new();
    let base = Url::parse(base_url).ok();

    for el in doc.select(&selector("a[href]")) {
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

        if links.len() == MAX_LINKS {
            return (links, true);
        }
        links.push(Link {
            text: truncate(&text, 100),
            url: full_url,
        });
    }

    (links, false)
}

/// Maximum code blocks returned per page
const MAX_CODE_BLOCKS: usize = 20;
/// Maximum characters per code block
const MAX_CODE_CHARS: usize = 5000;

fn extract_code_blocks(doc: &Html) -> (Vec<CodeBlock>, bool) {
    let mut blocks = Vec::new();
    let mut truncated = false;
    let mut seen_sources = HashSet::new();

    // Look for <pre><code> blocks first (more specific), then <pre>
    for selector_str in ["pre code", "pre"] {
        for el in doc.select(&selector(selector_str)) {
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

            truncated |= source.len() > MAX_CODE_CHARS;
            if blocks.len() == MAX_CODE_BLOCKS {
                truncated = true;
                break;
            }
            blocks.push(CodeBlock {
                lang,
                source: cut_at_word(&source, MAX_CODE_CHARS),
            });
        }
    }

    (blocks, truncated)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{all_content, fetch, fetch_with_profile, ref_cmd, serve};
    use crate::text::floor_char_boundary;
    use std::process::ExitCode;

    #[test]
    fn only_another_host_is_a_redirect() {
        let go = |from: &str, to: Option<&str>| redirect_target(from, to.map(String::from));
        let to = Some("https://other.example/x");
        assert_eq!(go("https://a.example/x", to).as_deref(), to);
        assert_eq!(go("https://a.example/x", Some("https://a.example/y")), None);
        // www. does not count, either way round
        assert_eq!(
            go("https://www.a.example/", Some("https://a.example/")),
            None
        );
        assert_eq!(
            go("https://a.example/", Some("https://www.a.example/")),
            None
        );
        // No final address, or an address without a host
        assert_eq!(go("https://a.example/", None), None);
        assert_eq!(go("https://a.example/", Some("about:blank")), None);
        assert_eq!(go("not an address", to), None);
    }

    #[test]
    fn tiny_code_snippets_are_not_code_blocks() {
        let page = parsed(
            "<html><body><main><p>Text.</p><pre>x = 1</pre><pre>   </pre></main></body></html>",
        );
        assert!(page.code.is_empty(), "{:?}", page.code);
    }

    #[test]
    fn at_most_twenty_code_blocks_are_kept_and_the_page_says_so() {
        let mut blocks = String::new();
        for i in 0..25 {
            std::fmt::Write::write_fmt(
                &mut blocks,
                format_args!("<pre><code>let value_{i} = {i};</code></pre>"),
            )
            .unwrap();
        }
        let page = parsed(&format!(
            "<html><body><main><p>Text.</p>{blocks}</main></body></html>"
        ));
        assert_eq!(page.code.len(), MAX_CODE_BLOCKS);
        assert!(page.truncated);
    }

    fn parsed(html: &str) -> Page {
        parse_page(
            html,
            "https://site.example/dir/page",
            &FetchOptions::default(),
        )
    }

    #[test]
    fn links_are_resolved_deduplicated_and_the_noise_skipped() {
        let page = parsed(
            "<html><body><main><h1>T</h1><p>Text.</p>\
             <a href='/docs/a'>Docs page</a><a href='/docs/a'>Docs page again</a>\
             <a href='#top'>To the top</a><a href='javascript:x()'>Run script</a>\
             <a href=''>Nothing at all</a><a href='/b'>go</a>\
             <a href='http://[bad'>Broken address</a></main></body></html>",
        );
        let urls: Vec<_> = page.links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(urls, ["https://site.example/docs/a", "http://[bad"]);
    }

    #[test]
    fn links_without_a_usable_page_address_stay_as_written() {
        let page = parse_page(
            "<html><body><main><p>Text.</p><a href='/x'>Relative link</a></main></body></html>",
            "not an address",
            &FetchOptions::default(),
        );
        assert_eq!(page.links[0].url, "/x");
    }

    #[test]
    fn only_the_first_links_are_kept_and_the_page_says_so() {
        let mut many = String::new();
        for i in 0..60 {
            std::fmt::Write::write_fmt(
                &mut many,
                format_args!("<a href='/p{i}'>Page number {i}</a>"),
            )
            .unwrap();
        }
        let page = parsed(&format!(
            "<html><body><main><p>Text.</p>{many}</main></body></html>"
        ));
        assert_eq!(page.links.len(), MAX_LINKS);
        assert!(
            page.alerts.iter().any(|a| a.contains("Links limited")),
            "{:?}",
            page.alerts
        );
    }

    #[test]
    fn raw_keeps_the_links_of_the_whole_page() {
        let html = "<html><body><nav><a href='/menu'>Menu entry</a></nav><main><p>Text.</p></main></body></html>";
        let raw = FetchOptions {
            raw: true,
            ..FetchOptions::default()
        };
        let page = parse_page(html, "https://site.example/", &raw);
        assert!(page.links.iter().any(|l| l.url.ends_with("/menu")));
        assert!(parsed(html).links.is_empty());
    }

    #[test]
    fn walls_are_named_in_the_alerts() {
        let walled = "<html><head><script type='application/ld+json'>{\"isAccessibleForFree\": false}</script></head><body><p>Subscribe.</p></body></html>";
        let page = parsed(walled);
        assert_eq!(page.status, PageStatus::Paywall);
        assert!(page.alerts.contains(&"Paywall detected".to_string()));
        let login = parsed(
            "<html><body><h1>Sign in</h1><form><input type='password'></form></body></html>",
        );
        assert_eq!(login.status, PageStatus::Login);
        assert!(login.alerts.contains(&"Login required".to_string()));
    }

    #[test]
    fn a_selector_that_is_not_css_is_said_so() {
        let options = FetchOptions {
            selector: Some("<<<".into()),
            ..FetchOptions::default()
        };
        let page = parse_page(
            "<html><body><p>Some text here.</p></body></html>",
            "https://a.example/",
            &options,
        );
        assert!(
            page.alerts.iter().any(|a| a.contains("is not valid CSS")),
            "{:?}",
            page.alerts
        );
        // The fallback returned the page's one paragraph
        assert_eq!(page.sections.len(), 1);
    }

    #[test]
    fn the_doi_comes_from_the_best_place_it_is_given() {
        let doi = |head: &str, body: &str| {
            parsed(&format!(
                "<html><head>{head}</head><body>{body}</body></html>"
            ))
            .doi
        };
        assert_eq!(
            doi("<meta name='citation_doi' content='10.1/x'>", "").as_deref(),
            Some("10.1/x")
        );
        assert_eq!(
            doi("<meta name='DC.identifier' content='10.2/y'>", "").as_deref(),
            Some("10.2/y")
        );
        assert_eq!(
            doi(
                "<meta name='DC.identifier' content='https://doi.org/10.3/z'>",
                ""
            )
            .as_deref(),
            Some("https://doi.org/10.3/z")
        );
        // An identifier that is not a DOI is ignored
        assert_eq!(
            doi("<meta name='DC.identifier' content='isbn:123'>", ""),
            None
        );
        let link = doi("", "<a href='https://doi.org/10.4/w'>the paper</a>");
        assert_eq!(link.as_deref(), Some("https://doi.org/10.4/w"));
        assert_eq!(doi("", "<p>none</p>"), None);
    }

    #[tokio::test]
    async fn several_urls_give_one_json_line_each() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (a, b) = (format!("{base}/report"), format!("{base}/fortune"));
        let (code, out, _) = ref_cmd(&["fetch", "--profile", "fresh", &a, &b]).await;
        assert_eq!(code, ExitCode::SUCCESS);
        let pages: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(pages.len(), 2, "{out}");
        assert!(pages.iter().all(|p| p["status"] == "ok"));
    }

    #[tokio::test]
    async fn a_redirect_to_another_host_is_a_redirect_page() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (code, page) = fetch(&[&format!("{base}/cross-host")]).await;
        assert_eq!(page["status"], "redirect", "{page}");
        assert!(
            page["alerts"]
                .to_string()
                .contains("Redirected to: http://localhost:"),
            "{page}"
        );
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn a_cookie_file_that_cannot_be_read_or_parsed_is_an_error() {
        let (code, _, err) = ref_cmd(&[
            "fetch",
            "--profile",
            "fresh",
            "--cookies",
            "/no/such/jar.txt",
            "http://127.0.0.1:9/",
        ])
        .await;
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("Failed to read cookie file"), "{err}");

        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("jar.txt");
        std::fs::write(&jar, "not\ta\tcookie\tline\n").unwrap();
        let line = [
            "fetch",
            "--profile",
            "fresh",
            "--cookies",
            jar.to_str().unwrap(),
            "http://127.0.0.1:9/",
        ];
        let (code, _, err) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("jar.txt"), "{err}");
    }

    #[tokio::test]
    async fn http_404_is_dead_and_exits_1() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (code, page) = fetch(&[&format!("{base}/missing")]).await;
        assert_eq!(page["status"], "dead");
        assert_eq!(page["http"], 404);
        assert_eq!(code, ExitCode::from(1));
    }

    #[tokio::test]
    async fn http_500_is_dead() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, page) = fetch(&[&format!("{base}/broken")]).await;
        assert_eq!(page["status"], "dead");
        assert_eq!(page["http"], 500);
    }

    #[tokio::test]
    async fn title_with_500_is_ok() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (code, page) = fetch(&[&format!("{base}/fortune")]).await;
        assert_eq!(page["status"], "ok");
        assert_eq!(page["http"], 200);
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn redirect_on_same_host_is_followed() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, page) = fetch(&[&format!("{base}/moved")]).await;
        assert_eq!(page["status"], "ok");
        assert_eq!(page["http"], 200);
        assert_eq!(page["title"], "Quarterly report");
    }

    #[tokio::test]
    async fn lists_tables_quotes_and_loose_text_are_kept() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, page) = fetch(&[&format!("{base}/report")]).await;
        let text = all_content(&page);
        for expected in [
            "Intro paragraph.",
            "- Revenue up 12%",
            "| Region | Q3 |\n| --- | --- |\n| North | 150 |",
            "> Strong quarter, said the CFO.",
            "Loose closing remark.",
        ] {
            assert!(text.contains(expected), "{expected:?} missing from {text}");
        }
        assert!(!text.contains("Home | Blog"), "nav kept: {text}");
        assert!(!text.contains("Site footer"), "footer kept: {text}");
    }

    #[tokio::test]
    async fn long_pages_are_cut_with_a_flag_or_kept_whole() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let url = format!("{base}/long");

        let (_, page) = fetch(&[&url]).await;
        assert_eq!(page["truncated"], true);
        assert_eq!(page["sections"][0]["truncated"], true);
        assert!(page["alerts"].to_string().contains("--max-chars 0"));

        let (_, full) = fetch(&["--max-chars", "0", &url]).await;
        assert!(full.get("truncated").is_none());
        let len = full["sections"][0]["content"].as_str().unwrap_or("").len();
        assert!(len > 16_000, "only {len} characters kept");
    }

    #[tokio::test]
    async fn content_loaded_by_fetch_after_load_is_seen() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, page) = fetch(&[&format!("{base}/late")]).await;
        assert!(
            all_content(&page).contains("arrived through fetch()"),
            "{page}"
        );
    }

    #[tokio::test]
    async fn walls_are_detected_only_when_real() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, article) = fetch(&[&format!("{base}/paywall-article")]).await;
        assert_eq!(article["status"], "ok");

        let (_, login) = fetch(&[&format!("{base}/login")]).await;
        assert_eq!(login["status"], "login");
    }

    #[tokio::test]
    async fn local_files_are_refused() {
        // Needs Chrome (ADR-004)
        let (code, page) = fetch(&["file:///etc/hostname"]).await;
        assert_eq!(page["status"], "dead");
        assert!(page["alerts"].to_string().contains("Only http and https"));
        assert_eq!(code, ExitCode::from(1));
    }

    #[tokio::test]
    async fn page_wrapped_in_a_form_keeps_its_content() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, page) = fetch(&[&format!("{base}/aspnet")]).await;
        assert!(
            all_content(&page).contains("Revenue rose this year."),
            "{page}"
        );
    }

    #[tokio::test]
    async fn empty_pages_and_unmatched_selectors_say_so() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, empty) = fetch(&[&format!("{base}/empty")]).await;
        assert!(
            empty["alerts"].to_string().contains("No readable content"),
            "{empty}"
        );

        let (_, page) = fetch(&["--selector", "#missing", &format!("{base}/report")]).await;
        assert!(
            page["alerts"].to_string().contains("matched nothing"),
            "{page}"
        );
    }

    #[tokio::test]
    async fn no_paywall_class_is_not_a_paywall() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, page) = fetch(&[&format!("{base}/no-paywall")]).await;
        assert_eq!(page["status"], "ok");
    }

    #[tokio::test]
    async fn shadow_dom_text_is_kept() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, page) = fetch(&[&format!("{base}/components")]).await;
        assert!(
            all_content(&page).contains("Text that lives in a shadow root."),
            "{page}"
        );
    }

    #[tokio::test]
    async fn hidden_overlay_is_not_a_paywall() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let (_, page) = fetch(&[&format!("{base}/hidden-overlay")]).await;
        assert_eq!(page["status"], "ok", "{page}");
    }

    #[tokio::test]
    async fn declared_user_agent_gets_through_and_403_says_why() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let url = format!("{base}/needs-declared-agent");

        let (code, page) = fetch(&["--user-agent", "Jane Doe jane@example.com", &url]).await;
        assert_eq!(page["status"], "ok", "{page}");
        assert!(all_content(&page).contains("declared reader"), "{page}");
        assert_eq!(code, ExitCode::SUCCESS);

        let (code, refused) = fetch(&[&url]).await;
        assert_eq!(refused["status"], "dead", "{refused}");
        assert_eq!(code, ExitCode::from(1));
        let alerts = refused["alerts"].to_string();
        assert!(alerts.contains("HTTP 403"), "{refused}");
        assert!(alerts.contains("--user-agent"), "{refused}");

        // A wrong custom agent is still refused, and the hint is not repeated
        let (_, wrong) = fetch(&["--user-agent", "nobody", &url]).await;
        assert_eq!(wrong["status"], "dead", "{wrong}");
        assert!(
            !wrong["alerts"].to_string().contains("try --user-agent"),
            "{wrong}"
        );
    }

    #[tokio::test]
    async fn a_pdf_that_cannot_be_read_stays_dead_and_says_why() {
        // Needs Chrome (ADR-004): Chrome calls it a PDF, the download is not one
        let base = serve();
        let (code, page) = fetch(&[&format!("{base}/text.pdf")]).await;
        assert_eq!(page["status"], "dead", "{page}");
        assert!(page["http"].is_null(), "{page}");
        let alerts = page["alerts"].to_string();
        assert!(alerts.contains("PDF detected"), "{alerts}");
        assert!(alerts.contains("PDF extraction failed"), "{alerts}");
        assert_eq!(code, ExitCode::from(1));
    }

    #[test]
    fn the_truncation_alert_names_only_the_limits_that_are_on() {
        let on = truncation_alert(&FetchOptions::default());
        assert!(on.contains("characters per section"), "{on}");
        assert!(
            on.contains(&format!("{DEFAULT_MAX_SECTIONS} sections")),
            "{on}"
        );
        let off = truncation_alert(&FetchOptions {
            max_chars: 0,
            max_sections: 0,
            ..FetchOptions::default()
        });
        assert!(!off.contains("characters per section"), "{off}");
        assert!(
            !off.contains(&format!("{DEFAULT_MAX_SECTIONS} sections")),
            "{off}"
        );
        assert!(off.contains("code blocks"), "{off}");
    }

    #[tokio::test]
    async fn signed_in_pdf_uses_the_cookie_file() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let dir = tempfile::tempdir().expect("tempdir");
        let jar = dir.path().join("cookies.txt");
        std::fs::write(&jar, "127.0.0.1\tFALSE\t/\tFALSE\t0\tsid\tletmein\n").expect("write jar");
        let url = format!("{base}/signed-in.pdf");

        let (code, page) = fetch(&["--cookies", jar.to_str().expect("path"), &url]).await;
        assert_eq!(page["status"], "ok", "{page}");
        assert!(all_content(&page).contains("signed-in readers"), "{page}");
        assert_eq!(code, ExitCode::SUCCESS);

        let (_, without) = fetch(&[&url]).await;
        assert_eq!(without["status"], "dead", "{without}");
    }

    #[tokio::test]
    async fn saved_profile_keeps_sign_ins_between_runs() {
        // Needs Chrome (ADR-004)
        let base = serve();
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = dir.path().join("profile");
        let profile = profile.to_str().expect("path");

        let (_, signed_in) = fetch_with_profile(profile, &[&format!("{base}/sign-in")]).await;
        assert_eq!(signed_in["status"], "ok", "{signed_in}");

        let (_, later) = fetch_with_profile(profile, &[&format!("{base}/whoami")]).await;
        assert!(
            all_content(&later).contains("session=kept-between-runs"),
            "{later}"
        );
        assert!(
            std::path::Path::new(profile).is_dir(),
            "saved profile was deleted"
        );

        let (_, fresh) = fetch(&[&format!("{base}/whoami")]).await;
        assert!(
            all_content(&fresh).contains("Cookie header: none"),
            "{fresh}"
        );
    }

    #[tokio::test]
    async fn everyday_chrome_profile_is_refused_with_advice() {
        // Needs no Chrome: refused before it starts
        let home = std::env::var("HOME").expect("HOME");
        let everyday = format!("{home}/.config/google-chrome");
        let line = ["fetch", "--profile", &everyday, "http://127.0.0.1:9/"];
        let (code, _, err) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::from(2));
        assert!(err.contains("ref login"), "{err}");
    }

    #[tokio::test]
    async fn a_tab_chrome_refuses_is_a_dead_page() {
        // Needs Chrome (ADR-004)
        let pool = crate::browser::tests::pool_that_cannot_set_up_tabs().await;
        let page = fetch_one(&pool, "http://127.0.0.1:9/", &FetchOptions::default()).await;
        assert_eq!(page.unwrap().status, PageStatus::Dead);
        pool.stop().await;
    }

    #[tokio::test]
    async fn a_chrome_failure_is_not_a_page() {
        // Needs Chrome (ADR-004). Before, it came back as a dead page.
        let pool = BrowserPool::new(1, Some(&crate::profile::ProfileChoice::Fresh))
            .await
            .unwrap();
        let trap = crate::browser::tests::serve_trap(&pool, "-KILL").await;
        let err = fetch_one(&pool, &trap, &FetchOptions::default())
            .await
            .unwrap_err();
        assert!(err.is::<crate::browser::ChromeFailed>(), "{err:#}");
        pool.stop().await;
    }

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
        let page = parse_page(html, "https://test.com", &FetchOptions::default());
        assert_eq!(page.status, PageStatus::Ok);
        assert_eq!(page.title, Some("Test Page".to_string()));
        assert_ne!(page.sections, Vec::<Section>::new());
    }

    fn ok_page() -> Page {
        parse_page(
            "<html><body><main><h1>Title</h1><p>Body text long enough.</p></main></body></html>",
            "https://test.com",
            &FetchOptions::default(),
        )
    }

    #[test]
    fn test_http_404_marks_page_dead() {
        let mut page = ok_page();
        apply_http_status(&mut page, Some(404));
        assert_eq!(page.status, PageStatus::Dead);
        assert_eq!(page.http, Some(404));
        assert_eq!(page.alerts.first().map(String::as_str), Some("HTTP 404"));
    }

    #[test]
    fn test_http_200_keeps_status() {
        let mut page = ok_page();
        apply_http_status(&mut page, Some(200));
        assert_eq!(page.status, PageStatus::Ok);
        assert_eq!(page.http, Some(200));
        assert_eq!(page.alerts, Vec::<String>::new());

        let mut page = ok_page();
        apply_http_status(&mut page, None);
        assert_eq!(page.status, PageStatus::Ok);
        assert_eq!(page.http, None);
    }

    #[test]
    fn test_title_with_500_is_not_an_error() {
        let page = parse_page(
            "<html><head><title>Fortune 500 Companies</title></head><body><p>Ranked list of companies.</p></body></html>",
            "https://test.com",
            &FetchOptions::default(),
    );
        assert_eq!(page.status, PageStatus::Ok);
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
    fn test_extract_sections_document_order() {
        let html = r"
            <html><body>
                <h1>Alpha</h1>
                <p>Content under alpha, long enough to be meaningful here.</p>
                <h2>Beta</h2>
                <p>Content under beta, also long enough to be meaningful.</p>
            </body></html>
        ";
        let page = parse_page(html, "https://test.com", &FetchOptions::default());
        assert!(!page.sections.is_empty(), "should have sections");
        let section = |name: &str| page.sections.iter().find(|s| s.heading == name);
        let alpha = section("Alpha").expect("should have Alpha section");
        assert!(
            alpha.content.contains("Content under alpha"),
            "{}",
            alpha.content
        );
        let beta = section("Beta").expect("should have Beta section");
        assert!(
            beta.content.contains("Content under beta"),
            "{}",
            beta.content
        );
    }

    #[test]
    fn test_extract_code() {
        let html =
            r#"<pre><code class="language-rust">fn main() { println!("hello"); }</code></pre>"#;
        let doc = Html::parse_document(html);
        let (blocks, truncated) = extract_code_blocks(&doc);
        assert!(!truncated);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang, Some("rust".to_string()));
    }

    // -------------------------------------------------------------------------
    // Selector Tests (v1.6.0)
    // -------------------------------------------------------------------------

    #[test]
    fn test_parse_page_with_selector() {
        let html = r#"
            <html><body>
                <nav><a href="/">Home</a></nav>
                <div id="target">
                    <h1>Targeted Heading</h1>
                    <p>This is the targeted content, long enough to be meaningful.</p>
                </div>
                <footer>Footer noise</footer>
            </body></html>
        "#;
        let page = parse_page(
            html,
            "https://test.com",
            &FetchOptions {
                selector: Some("#target".into()),
                ..FetchOptions::default()
            },
        );
        assert_ne!(page.sections, Vec::<Section>::new());
        let has_targeted = page
            .sections
            .iter()
            .any(|s| s.heading.contains("Targeted") || s.content.contains("targeted content"));
        assert!(
            has_targeted,
            "Should extract targeted element content, got: {:?}",
            page.sections
        );
    }

    #[test]
    fn test_parse_page_selector_fallback() {
        let html = r"
            <html><body>
                <h1>Normal Heading</h1>
                <p>Normal content that is long enough to be meaningful here.</p>
            </body></html>
        ";
        // Selector that matches nothing — should fall back gracefully
        let page = parse_page(
            html,
            "https://test.com",
            &FetchOptions {
                selector: Some(".nonexistent-class".into()),
                ..FetchOptions::default()
            },
        );
        assert!(
            !page.sections.is_empty(),
            "Should fall back to normal extraction"
        );
    }
}
