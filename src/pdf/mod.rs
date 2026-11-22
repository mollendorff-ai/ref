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

    #[command(flatten)]
    pub proxy: crate::proxy::ProxyArgs,
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
pub async fn run_pdf(args: &PdfArgs, out: &mut dyn std::io::Write) -> Result<bool> {
    let file_count = args.files.len();
    eprintln!(
        "Extracting {} PDF{}...",
        file_count,
        if file_count == 1 { "" } else { "s" }
    );

    let headers = crate::browser::DownloadHeaders {
        proxy: args.proxy.choice()?,
        ..Default::default()
    };
    let mut results: Vec<PdfPage> = Vec::new();

    for file in &args.files {
        eprintln!("  -> {file}");
        let page = if is_url(file) {
            extract_pdf_from_url_with(file, &headers).await
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
        writeln!(out, "{}", serde_json::to_string(&results[0])?)?;
    } else {
        for page in &results {
            writeln!(out, "{}", serde_json::to_string(page)?)?;
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

/// What a PDF download may take: time to connect, time in all, and bytes
#[derive(Debug, Clone, Copy)]
pub(crate) struct DownloadLimits {
    pub connect: std::time::Duration,
    pub total: std::time::Duration,
    pub max_bytes: usize,
}

impl Default for DownloadLimits {
    fn default() -> Self {
        Self {
            connect: std::time::Duration::from_secs(15),
            total: std::time::Duration::from_mins(1),
            max_bytes: 100 * 1024 * 1024,
        }
    }
}

impl DownloadLimits {
    /// What a timed-out download says
    fn timed_out(&self) -> String {
        format!("Download timed out (limit {} s)", self.total.as_secs())
    }

    /// What a download that is too large says
    fn too_big(&self) -> String {
        format!("PDF is larger than {} MB", self.max_bytes / 1024 / 1024)
    }
}

/// Download and extract a PDF, sending the given user agent and cookies
pub(crate) async fn extract_pdf_from_url_with(
    url: &str,
    headers: &crate::browser::DownloadHeaders,
) -> PdfPage {
    extract_pdf_from_url_within(url, headers, &DownloadLimits::default()).await
}

/// [`extract_pdf_from_url_with`] with other limits on the download
pub(crate) async fn extract_pdf_from_url_within(
    url: &str,
    headers: &crate::browser::DownloadHeaders,
    limits: &DownloadLimits,
) -> PdfPage {
    if let Err(e) = crate::guard::check_web_url(url) {
        return error_page(url, &e);
    }
    match download_pdf(url, headers, limits).await {
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
    limits: &DownloadLimits,
) -> Result<Vec<u8>, String> {
    let user_agent = if headers.user_agent.is_empty() {
        concat!("mollendorff-ref/", env!("CARGO_PKG_VERSION")).to_string()
    } else {
        headers.user_agent.clone()
    };
    let mut builder = reqwest::Client::builder()
        .connect_timeout(limits.connect)
        .timeout(limits.total)
        .user_agent(user_agent);
    if let Some(proxy) = &headers.proxy {
        builder = builder.proxy(
            proxy
                .for_download()
                .map_err(|e| format!("Download failed: {e}"))?,
        );
    }
    let client = builder
        .build()
        .map_err(|e| format!("Download failed: {e}"))?;
    let mut request = client.get(url);
    if let Some(cookie) = &headers.cookie {
        request = request.header(reqwest::header::COOKIE, cookie);
    }
    let mut resp = request.send().await.map_err(|e| {
        if e.is_timeout() {
            limits.timed_out()
        } else {
            format!("Download failed: {e}")
        }
    })?;
    if !resp.status().is_success() {
        return Err(format!("Download failed: HTTP {}", resp.status().as_u16()));
    }
    if resp
        .content_length()
        .is_some_and(|n| n > limits.max_bytes as u64)
    {
        return Err(limits.too_big());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        if e.is_timeout() {
            limits.timed_out()
        } else {
            format!("Failed to read response: {e}")
        }
    })? {
        if bytes.len() + chunk.len() > limits.max_bytes {
            return Err(limits.too_big());
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

/// Run `work`, whose error is the PDF's failure to be read. pdf-extract can
/// also panic on malformed files; that is reported as an error too, so it
/// cannot take down the whole command or the MCP server.
fn guarded<T>(
    work: impl FnOnce() -> Result<T, String> + std::panic::UnwindSafe,
) -> Result<T, String> {
    std::panic::catch_unwind(work).unwrap_or_else(|_| Err("malformed PDF".to_string()))
}

/// Extract a PDF held in memory; `name` gives the fallback title.
fn extract_pdf_bytes(bytes: &[u8], file_url: String, name: &Path) -> PdfPage {
    let (text, info) = match guarded(|| extract_layout_text(bytes).map_err(|e| e.to_string())) {
        Ok(extracted) => extracted,
        Err(why) => return error_page(&file_url, &format!("PDF extraction failed: {why}")),
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
    use crate::test_server::{pdf_with_lines, ref_cmd, serve};
    use crate::text::truncate;
    use std::process::ExitCode;

    #[test]
    fn a_reader_that_fails_or_panics_gives_an_error_not_a_crash() {
        assert_eq!(guarded(|| Ok::<_, String>(7)), Ok(7));
        assert_eq!(
            guarded(|| Err::<u8, _>("bad xref".to_string())),
            Err("bad xref".to_string())
        );
        let panicked = guarded(|| -> Result<u8, String> { panic!("lopdf gave up") });
        assert_eq!(panicked, Err("malformed PDF".to_string()));
    }

    #[tokio::test]
    async fn a_download_client_that_cannot_be_built_is_a_failed_download() {
        // A user agent with a line break is not a valid header value
        let headers = crate::browser::DownloadHeaders {
            user_agent: "bad\nagent".into(),
            cookie: None,
            proxy: None,
        };
        let page = extract_pdf_from_url_within("http://127.0.0.1:9/a.pdf", &headers, &SHORT).await;
        assert!(alert_of(&page).contains("Download failed"), "{page:?}");
    }

    const SHORT: DownloadLimits = DownloadLimits {
        connect: std::time::Duration::from_secs(5),
        total: std::time::Duration::from_millis(700),
        max_bytes: 2000,
    };

    async fn download(url: &str) -> PdfPage {
        let headers = crate::browser::DownloadHeaders::default();
        extract_pdf_from_url_within(url, &headers, &SHORT).await
    }

    fn alert_of(page: &PdfPage) -> String {
        assert_eq!(page.base.status, PageStatus::Dead);
        page.base.alerts.join(" ")
    }

    #[tokio::test]
    async fn a_download_that_goes_wrong_says_how() {
        let base = serve();
        let cases = [
            ("/slow.pdf", "Download timed out"),
            ("/stall.pdf", "Download timed out"),
            ("/big.pdf", "larger than"),
            ("/chunked.pdf", "larger than"),
            ("/cut.pdf", "Failed to read response"),
            ("/text.pdf", "PDF extraction failed"),
            ("/signed-in.pdf", "HTTP 403"),
        ];
        for (path, expected) in cases {
            let page = download(&format!("{base}{path}")).await;
            let alert = alert_of(&page);
            assert!(alert.contains(expected), "{path}: {alert}");
        }
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let alert = alert_of(&download(&format!("http://{closed}/a.pdf")).await);
        assert!(alert.contains("Download failed"), "{alert}");
        let alert = alert_of(&download("file:///etc/hostname").await);
        assert!(alert.contains("Only http and https"), "{alert}");
    }

    #[tokio::test]
    async fn a_pdf_from_a_url_is_downloaded_and_extracted() {
        let base = serve();
        let headers = crate::browser::DownloadHeaders::default();
        let page = extract_pdf_from_url_with(&format!("{base}/report.pdf"), &headers).await;
        assert_eq!(page.base.status, PageStatus::Ok);
        assert_eq!(page.base.title.as_deref(), Some("Regional report"));
    }

    #[tokio::test]
    async fn several_files_and_urls_give_one_json_line_each() {
        let base = serve();
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/report.pdf");
        let url = format!("{base}/report.pdf");
        let (code, out, _) = ref_cmd(&["pdf", fixture, &url, "/no/such/file.pdf"]).await;
        assert_eq!(code, ExitCode::from(1));
        let pages: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(pages.len(), 3, "{out}");
        assert_eq!(
            (pages[0]["status"].as_str(), pages[1]["status"].as_str()),
            (Some("ok"), Some("ok"))
        );
        assert_eq!(pages[2]["status"], "dead");

        let (code, out, _) = ref_cmd(&["pdf", fixture]).await;
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(out.starts_with("{\"url\":\"file://"), "{out}");
    }

    #[test]
    fn a_file_that_cannot_be_used_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let alert = |path: &Path| alert_of(&extract_pdf(&path.to_path_buf()));
        assert_eq!(alert(&dir.path().join("missing.pdf")), "File not found");
        // The empty path has no absolute form
        assert_eq!(alert(Path::new("")), "File not found");
        // A folder cannot be read as a file
        assert!(alert(dir.path()).contains("Failed to read file"));

        let garbage = dir.path().join("garbage.pdf");
        std::fs::write(&garbage, b"%PDF-1.4 this is not really a pdf").unwrap();
        assert!(alert(&garbage).contains("PDF extraction failed"));

        // Not even the start of a PDF: reading fails instead of panicking
        let text = dir.path().join("text.pdf");
        std::fs::write(&text, b"hello").unwrap();
        assert!(alert(&text).contains("PDF extraction failed"));

        let blank = dir.path().join("blank.pdf");
        std::fs::write(&blank, pdf_with_lines(&[])).unwrap();
        assert_eq!(alert(&blank), "PDF contains no extractable text");
    }

    #[test]
    fn a_long_section_is_cut_and_the_page_says_so() {
        let long = "word ".repeat(2500);
        let bytes = pdf_with_lines(&["1. Introduction", &long]);
        let page = extract_pdf_bytes(&bytes, "file:///long.pdf".into(), Path::new("long.pdf"));
        assert_eq!(page.base.status, PageStatus::Ok);
        assert!(page.base.truncated);
        assert!(
            page.base.alerts[0].starts_with("Content truncated"),
            "{:?}",
            page.base.alerts
        );
    }

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
