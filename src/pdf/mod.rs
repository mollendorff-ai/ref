//! pdf command: Extract text from PDF files
//!
//! Local extraction, no external APIs.
//! Output matches fetch command structure for consistency.
//!
//! - [`layout`]: text and metadata from the PDF, keeping column gaps
//! - [`tables`]: tables found in that text
//! - [`headings`] and [`sections`]: headings, and sections between them
//! - [`fields`]: title, author, date, DOI, links and code

mod fields;
mod headings;
mod layout;
mod sections;
mod tables;

pub use tables::Table;

use crate::fetch::{Page, PageStatus};
use anyhow::Result;
use clap::Args;
use fields::{
    extract_author, extract_code, extract_date, extract_doi, extract_links, extract_title,
};
use layout::extract_layout_text;
use sections::{parse_sections, PDF_MAX_SECTIONS, PDF_SECTION_CHARS};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tables::detect_tables;

#[derive(Args)]
pub struct PdfArgs {
    /// PDF files or URLs to extract
    #[arg(required = true)]
    pub files: Vec<String>,
}

fn is_url(path: &str) -> bool {
    path.starts_with("http://") || path.starts_with("https://")
}

/// Run the PDF extraction command.
///
/// # Errors
///
/// Returns an error if JSON serialization or network request fails.
/// Returns `Ok(false)` when any file could not be read or downloaded.
pub async fn run_pdf(args: &PdfArgs) -> Result<bool> {
    let file_count = args.files.len();
    eprintln!(
        "Extracting {} PDF{}...",
        file_count,
        if file_count == 1 { "" } else { "s" }
    );

    let mut results: Vec<PdfPage> = Vec::new();

    for file in &args.files {
        eprintln!("  -> {file}");
        let page = if is_url(file) {
            extract_pdf_from_url(file).await
        } else {
            extract_pdf(&PathBuf::from(file))
        };
        results.push(page);
    }

    let ok_count = results
        .iter()
        .filter(|p| p.base.status == PageStatus::Ok)
        .count();

    // Output compact JSON (one line per page for multiple, or single object)
    if results.len() == 1 {
        println!("{}", serde_json::to_string(&results[0])?);
    } else {
        for page in &results {
            println!("{}", serde_json::to_string(page)?);
        }
    }

    eprintln!("Done: {ok_count}/{file_count} OK");
    Ok(results.iter().all(|p| p.base.status != PageStatus::Dead))
}

/// Extended page with tables
#[derive(Debug, Serialize, Clone)]
pub struct PdfPage {
    #[serde(flatten)]
    pub base: Page,
    /// Detected tables
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<Table>,
}

/// Largest PDF downloaded from a URL (bytes)
const MAX_PDF_DOWNLOAD: usize = 100 * 1024 * 1024;

/// Connect timeout for PDF downloads
const PDF_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Total time allowed for one PDF download
const PDF_DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

pub(crate) async fn extract_pdf_from_url(url: &str) -> PdfPage {
    extract_pdf_from_url_with(url, &crate::browser::DownloadHeaders::default()).await
}

/// Download and extract a PDF, sending the given user agent and cookies
pub(crate) async fn extract_pdf_from_url_with(
    url: &str,
    headers: &crate::browser::DownloadHeaders,
) -> PdfPage {
    if let Err(e) = crate::guard::check_web_url(url) {
        return error_page(url, &e);
    }
    match download_pdf(url, headers).await {
        Ok(bytes) => {
            let name = url
                .split(['?', '#'])
                .next()
                .and_then(|u| u.rsplit('/').next())
                .unwrap_or("document.pdf");
            extract_pdf_bytes(&bytes, url.to_string(), Path::new(name))
        }
        Err(e) => error_page(url, &e),
    }
}

/// Download with timeouts and a size cap checked while streaming, so a huge or
/// stalled response cannot exhaust memory or hang the caller.
async fn download_pdf(
    url: &str,
    headers: &crate::browser::DownloadHeaders,
) -> Result<Vec<u8>, String> {
    let too_big = || format!("PDF is larger than {} MB", MAX_PDF_DOWNLOAD / 1024 / 1024);
    let user_agent = if headers.user_agent.is_empty() {
        concat!("mollendorff-ref/", env!("CARGO_PKG_VERSION")).to_string()
    } else {
        headers.user_agent.clone()
    };
    let client = reqwest::Client::builder()
        .connect_timeout(PDF_CONNECT_TIMEOUT)
        .timeout(PDF_DOWNLOAD_TIMEOUT)
        .user_agent(user_agent)
        .build()
        .map_err(|e| format!("Download failed: {e}"))?;
    let mut request = client.get(url);
    if let Some(cookie) = &headers.cookie {
        request = request.header(reqwest::header::COOKIE, cookie);
    }
    let mut resp = request.send().await.map_err(|e| {
        if e.is_timeout() {
            format!(
                "Download timed out (limit {} s)",
                PDF_DOWNLOAD_TIMEOUT.as_secs()
            )
        } else {
            format!("Download failed: {e}")
        }
    })?;
    if !resp.status().is_success() {
        return Err(format!("Download failed: HTTP {}", resp.status().as_u16()));
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_PDF_DOWNLOAD as u64)
    {
        return Err(too_big());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        if e.is_timeout() {
            format!(
                "Download timed out (limit {} s)",
                PDF_DOWNLOAD_TIMEOUT.as_secs()
            )
        } else {
            format!("Failed to read response: {e}")
        }
    })? {
        if bytes.len() + chunk.len() > MAX_PDF_DOWNLOAD {
            return Err(too_big());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) fn extract_pdf(path: &PathBuf) -> PdfPage {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.clone());
    let file_url = format!("file://{}", absolute.display());

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return error_page(&file_url, "File not found")
        }
        Err(e) => return error_page(&file_url, &format!("Failed to read file: {e}")),
    };
    extract_pdf_bytes(&bytes, file_url, path)
}

/// Extract a PDF held in memory; `name` gives the fallback title.
fn extract_pdf_bytes(bytes: &[u8], file_url: String, name: &Path) -> PdfPage {
    // pdf-extract can panic on malformed files; keep that from taking down
    // the whole command or the MCP server.
    let (text, info) = match std::panic::catch_unwind(|| extract_layout_text(bytes)) {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            return error_page(&file_url, &format!("PDF extraction failed: {e}"));
        }
        Err(_) => return error_page(&file_url, "PDF extraction failed: malformed PDF"),
    };

    if text.is_empty() {
        return error_page(&file_url, "PDF contains no extractable text");
    }

    let tables = detect_tables(&text);

    let sections = parse_sections(&text);

    let links = extract_links(&text);

    // Title: the PDF's metadata title, else its first line, else the file name
    let title = info.title.or_else(|| extract_title(&text, name));

    let chars: usize = sections
        .iter()
        .map(|s| s.content.chars().count() + s.heading.chars().count())
        .sum();

    let truncated = sections.len() == PDF_MAX_SECTIONS || sections.iter().any(|s| s.truncated);
    let alerts = if truncated {
        vec![format!(
            "Content truncated: PDF sections are limited to {PDF_SECTION_CHARS} characters and {PDF_MAX_SECTIONS} sections"
        )]
    } else {
        vec![]
    };

    PdfPage {
        base: Page {
            url: file_url,
            status: PageStatus::Ok,
            http: None,
            title,
            site: None,
            author: info.author.or_else(|| extract_author(&text)),
            date: extract_date(&text),
            doi: extract_doi(&text),
            sections,
            links,
            code: extract_code(&text),
            alerts,
            chars,
            truncated,
        },
        tables,
    }
}

fn error_page(url: &str, error: &str) -> PdfPage {
    PdfPage {
        base: Page {
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
        },
        tables: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::truncate;

    /// The fixture made by `tests/fixtures/make_report_pdf.py`: a first-line
    /// heading, a table drawn cell by cell with right-aligned numbers and
    /// spaced-out rows, and a heading drawn as number + title.
    fn report_fixture() -> PdfPage {
        let bytes = include_bytes!("../../tests/fixtures/report.pdf");
        extract_pdf_bytes(
            bytes,
            "file:///report.pdf".to_string(),
            Path::new("report.pdf"),
        )
    }

    #[test]
    fn real_pdf_table_with_right_aligned_columns_is_detected() {
        let page = report_fixture();
        assert_eq!(page.base.status, PageStatus::Ok);
        assert_eq!(page.tables.len(), 1, "{:?}", page.tables);
        let table = &page.tables[0];
        assert_eq!(
            table.headers.as_deref(),
            Some(&["Region", "Q2", "Q3", "Change"].map(String::from)[..])
        );
        let rows: Vec<Vec<&str>> = table
            .rows
            .iter()
            .map(|r| r.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            rows,
            [
                ["North", "140", "150", "7%"],
                ["South", "190", "200", "5%"],
                ["East", "80", "90", "12%"],
                ["West", "60", "64", "6%"],
            ]
        );
        assert!(table.markdown.starts_with("|Region|Q2|Q3|Change|\n|---|"));
    }

    #[test]
    fn real_pdf_headings_become_sections() {
        let page = report_fixture();
        let headings: Vec<(u8, &str)> = page
            .base
            .sections
            .iter()
            .map(|s| (s.level, s.heading.as_str()))
            .collect();
        assert_eq!(
            headings,
            [
                (1, "1. Introduction"),
                (1, "2. Results"),
                (2, "2.1 Notes on the data"),
                (1, "3. Conclusion"),
            ]
        );
        // Column gaps are kept for tables only; section text has single spaces
        let results = &page.base.sections[1].content;
        assert!(results.contains("Region Q2 Q3 Change"), "{results}");
        assert!(!results.contains("  "), "{results}");
        assert_eq!(
            page.base.sections[3].content,
            "Growth was strongest in the East."
        );
    }

    #[test]
    fn real_pdf_metadata_title_is_used_and_placeholders_are_not() {
        let page = report_fixture();
        // Info /Title is "Regional report"; the first line is a heading, not a title
        assert_eq!(page.base.title.as_deref(), Some("Regional report"));
        // Info /Author is reportlab's "anonymous", and "by region" in a sentence is not an author
        assert_eq!(page.base.author, None);
    }

    #[test]
    fn signed_in_fixture_still_extracts() {
        let bytes = include_bytes!("../../tests/fixtures/signed-in.pdf");
        let page = extract_pdf_bytes(bytes, "file:///s.pdf".to_string(), Path::new("s.pdf"));
        assert_eq!(page.base.status, PageStatus::Ok);
        assert_eq!(page.base.sections.len(), 1);
        assert_eq!(page.base.sections[0].heading, "1. SUMMARY");
        assert!(page.base.sections[0].content.contains("signed-in readers"));
    }

    #[test]
    fn not_a_pdf_is_an_error_page() {
        let page = extract_pdf_bytes(
            b"<html>nope</html>",
            "file:///x.pdf".into(),
            Path::new("x.pdf"),
        );
        assert_eq!(page.base.status, PageStatus::Dead);
        assert!(
            page.base.alerts[0].starts_with("PDF extraction failed"),
            "{:?}",
            page.base.alerts
        );
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello...");
    }

    // URL Detection Tests (v1.6.0)

    #[test]
    fn test_is_url() {
        assert!(is_url("https://example.com/doc.pdf"));
        assert!(is_url("http://example.com/doc.pdf"));
        assert!(!is_url("/local/path/doc.pdf"));
        assert!(!is_url("relative/path.pdf"));
        assert!(!is_url("C:\\Windows\\file.pdf"));
    }
}
