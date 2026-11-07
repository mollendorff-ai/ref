//! pdf command: Extract text from PDF files
//!
//! Local extraction, no external APIs.
//! Output matches fetch command structure for consistency.
//!
//! v1.2.0: Table detection and improved heading detection

use crate::fetch::{CodeBlock, Link, Page, PageStatus, Section};
use crate::text::truncate;
use anyhow::Result;
use clap::Args;
use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

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

// ============================================================================
// Table Detection
// ============================================================================

/// A detected table from PDF text
#[derive(Debug, Serialize, Clone)]
pub struct Table {
    /// Header row (if detected)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<Vec<String>>,
    /// Data rows
    pub rows: Vec<Vec<String>>,
    /// Table as markdown (for LLM consumption)
    pub markdown: String,
}

/// Configuration for table detection
#[derive(Clone)]
struct TableConfig {
    /// Minimum gap (spaces) to be a column separator
    min_gap_chars: usize,
    /// Minimum rows to be considered a table
    min_rows: usize,
    /// Minimum columns to be considered a table
    min_columns: usize,
    /// Maximum variance in column positions across rows
    column_tolerance: usize,
}

impl Default for TableConfig {
    fn default() -> Self {
        Self {
            min_gap_chars: 2,
            min_rows: 2,
            min_columns: 2,
            column_tolerance: 3,
        }
    }
}

/// Detect tables in text using whitespace column analysis.
///
/// Blank lines inside a table do not end it: PDF text often has an empty line
/// between rows when the rows are spaced out on the page.
fn detect_tables(text: &str) -> Vec<Table> {
    let config = TableConfig::default();
    let lines: Vec<&str> = text.lines().collect();
    let mut tables = Vec::new();
    // (first table line, last table line, number of table lines)
    let mut region: Option<(usize, usize, usize)> = None;
    let mut blank_since_last = false;

    let close = |region: Option<(usize, usize, usize)>, tables: &mut Vec<Table>| {
        if let Some((start, last, count)) = region {
            if count >= config.min_rows {
                if let Some(table) = parse_table_region(&lines[start..=last], &config) {
                    tables.push(table);
                }
            }
        }
    };

    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            blank_since_last = true;
            continue;
        }
        if is_potential_table_line(line, &config) && !is_heading_line(line) {
            // After a blank line, only a row with as many cells continues the table
            if let Some((_, last, _)) = region {
                if blank_since_last && cell_runs(line).len() != cell_runs(lines[last]).len() {
                    close(region.take(), &mut tables);
                }
            }
            region = Some(match region {
                Some((start, _, count)) => (start, i, count + 1),
                None => (i, i, 1),
            });
        } else {
            close(region.take(), &mut tables);
        }
        blank_since_last = false;
    }
    close(region, &mut tables);

    // Limit tables
    tables.truncate(20);
    tables
}

/// The cells of a line: text runs separated by two or more spaces
fn cell_runs(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut spaces = 0;
    for c in line.trim().chars() {
        if c == ' ' || c == '\t' {
            spaces += 1;
            continue;
        }
        if spaces >= 2 && !cell.is_empty() {
            cells.push(std::mem::take(&mut cell));
        } else if spaces == 1 {
            cell.push(' ');
        }
        spaces = 0;
        cell.push(c);
    }
    if !cell.is_empty() {
        cells.push(cell);
    }
    cells
}

/// A line that is a confident heading ("2.1  Notes on the data") is never a
/// table row, even when a gap separates its number from its title
fn is_heading_line(line: &str) -> bool {
    find_gaps(line.trim(), 2).len() < 2
        && detect_heading(line.trim(), &HeadingConfig::default())
            .is_some_and(|h| h.confidence >= 0.8)
}

/// Check if a line looks like it could be part of a table
fn is_potential_table_line(line: &str, config: &TableConfig) -> bool {
    let trimmed = line.trim();

    // Skip empty lines and very short lines
    if trimmed.len() < 5 {
        return false;
    }

    // Skip lines that look like prose (sentences)
    if is_prose_line(trimmed) {
        return false;
    }

    // Count significant whitespace gaps (2+ spaces)
    let gaps = find_gaps(line, config.min_gap_chars);

    // Table lines have multiple columns separated by gaps
    gaps.len() >= config.min_columns - 1
}

/// Find positions of whitespace gaps in a line
fn find_gaps(line: &str, min_gap: usize) -> Vec<usize> {
    let mut gaps = Vec::new();
    let mut gap_start: Option<usize> = None;
    let chars: Vec<char> = line.chars().collect();

    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' || c == '\t' {
            if gap_start.is_none() {
                gap_start = Some(i);
            }
        } else if let Some(start) = gap_start {
            let gap_len = i - start;
            if gap_len >= min_gap {
                gaps.push(start + gap_len / 2); // Middle of gap
            }
            gap_start = None;
        }
    }

    gaps
}

/// Check if a line looks like prose (a sentence)
fn is_prose_line(line: &str) -> bool {
    let trimmed = line.trim();

    // Very long lines are likely prose
    if trimmed.len() > 150 {
        return true;
    }

    // Lines ending with sentence punctuation
    if trimmed.ends_with('.') && !trimmed.ends_with("..") {
        let word_count = trimmed.split_whitespace().count();
        if word_count >= 6 {
            return true;
        }
    }

    // Lines with typical prose patterns
    let has_articles = trimmed.contains(" the ")
        || trimmed.contains(" a ")
        || trimmed.contains(" an ")
        || trimmed.starts_with("The ")
        || trimmed.starts_with("A ");

    has_articles && trimmed.split_whitespace().count() > 6
}

/// Parse a region of lines into a table
fn parse_table_region(lines: &[&str], config: &TableConfig) -> Option<Table> {
    if lines.len() < config.min_rows {
        return None;
    }

    // When every row has the same number of cells, split each row on its own
    // gaps: this keeps right-aligned columns (numbers) intact
    let split: Vec<Vec<String>> = lines
        .iter()
        .map(|l| cell_runs(l))
        .filter(|cells| !cells.is_empty())
        .collect();
    let same_count = split.first().is_some_and(|first| {
        first.len() >= config.min_columns && split.iter().all(|r| r.len() == first.len())
    });

    let rows: Vec<Vec<String>> = if same_count {
        split
    } else {
        // Rows with empty cells: find consistent column boundaries across all lines
        let column_positions = find_column_positions(lines, config)?;
        if column_positions.len() < config.min_columns - 1 {
            return None;
        }
        lines
            .iter()
            .map(|line| extract_cells(line, &column_positions))
            .filter(|cells| cells.iter().any(|c| !c.is_empty()))
            .collect()
    };

    if rows.len() < config.min_rows {
        return None;
    }

    // Detect header row (first row is header if it differs from data pattern)
    let (headers, data_rows) = detect_header_row(&rows);

    // Generate markdown
    let markdown = generate_table_markdown(headers.as_ref(), &data_rows);

    Some(Table {
        headers,
        rows: data_rows,
        markdown,
    })
}

/// Find consistent column separator positions across lines
fn find_column_positions(lines: &[&str], config: &TableConfig) -> Option<Vec<usize>> {
    // Collect all gap positions from all lines
    let mut all_gaps: Vec<Vec<usize>> = Vec::new();

    for line in lines {
        let gaps = find_gaps(line, config.min_gap_chars);
        if !gaps.is_empty() {
            all_gaps.push(gaps);
        }
    }

    if all_gaps.is_empty() {
        return None;
    }

    // Find gap positions that appear in most lines (with tolerance)
    let mut position_counts: Vec<(usize, usize)> = Vec::new(); // (position, count)

    for gaps in &all_gaps {
        for &gap_pos in gaps {
            // Check if this position is close to an existing one
            let mut found = false;
            for (pos, count) in &mut position_counts {
                if pos.abs_diff(gap_pos) <= config.column_tolerance {
                    *count += 1;
                    // Update position to running average
                    *pos = (*pos * (*count - 1) + gap_pos) / *count;
                    found = true;
                    break;
                }
            }
            if !found {
                position_counts.push((gap_pos, 1));
            }
        }
    }

    // Keep positions that appear in at least half the lines
    let threshold = all_gaps.len() / 2;
    let mut consistent_positions: Vec<usize> = position_counts
        .into_iter()
        .filter(|(_, count)| *count >= threshold.max(2))
        .map(|(pos, _)| pos)
        .collect();

    consistent_positions.sort_unstable();

    if consistent_positions.is_empty() {
        None
    } else {
        Some(consistent_positions)
    }
}

/// Extract cells from a line using column positions
fn extract_cells(line: &str, column_positions: &[usize]) -> Vec<String> {
    let mut cells = Vec::new();
    let chars: Vec<char> = line.chars().collect();

    let mut start = 0;
    for &pos in column_positions {
        if pos < chars.len() {
            let cell: String = chars[start..pos].iter().collect();
            cells.push(cell.trim().to_string());
            start = pos;
        }
    }

    // Last cell
    if start < chars.len() {
        let cell: String = chars[start..].iter().collect();
        cells.push(cell.trim().to_string());
    }

    cells
}

/// Detect if the first row is a header
fn detect_header_row(rows: &[Vec<String>]) -> (Option<Vec<String>>, Vec<Vec<String>>) {
    if rows.is_empty() {
        return (None, Vec::new());
    }

    if rows.len() == 1 {
        return (None, rows.to_vec());
    }

    let first_row = &rows[0];
    let second_row = &rows[1];

    // Heuristics for header detection
    let mut header_score = 0;

    // Check if first row has no numeric values but second row does
    let first_has_numbers = first_row.iter().any(|c| has_numeric_value(c));
    let second_has_numbers = second_row.iter().any(|c| has_numeric_value(c));

    if !first_has_numbers && second_has_numbers {
        header_score += 3;
    }

    // Check if first row cells are shorter (headers tend to be concise)
    // Compare averages using integer cross-multiplication to avoid float casts:
    // first_avg < second_avg * 0.8  =>  first_total * second_count * 10 < second_total * first_count * 8
    let first_total: usize = first_row.iter().map(String::len).sum();
    let second_total: usize = second_row.iter().map(String::len).sum();
    let first_count = first_row.len().max(1);
    let second_count = second_row.len().max(1);

    if first_total * second_count * 10 < second_total * first_count * 8 {
        header_score += 1;
    }

    // Check if the first row is styled more like a header than the second
    // (ALL CAPS over Title Case over lower case); rows styled alike say nothing
    if caps_style(first_row) > caps_style(second_row) {
        header_score += 2;
    }

    if header_score >= 3 {
        (Some(first_row.clone()), rows[1..].to_vec())
    } else {
        (None, rows.to_vec())
    }
}

/// Check if a cell is a number (amounts, percentages, counts): it starts with
/// a digit once signs, currency and brackets are removed.
/// Labels with a digit inside ("Q3", "FY2026") are not numbers.
fn has_numeric_value(s: &str) -> bool {
    let mut cleaned = s
        .chars()
        .filter(|c| !['$', '€', '£', ',', '%', '(', ')', '-', '+', ' '].contains(c))
        .skip_while(|c| *c == '.')
        .peekable();
    cleaned.peek().is_some_and(char::is_ascii_digit)
}

/// How header-like a row's capitals are: 2 = every word ALL CAPS,
/// 1 = some word Title Case, 0 = neither
fn caps_style(row: &[String]) -> u8 {
    let words: Vec<&String> = row
        .iter()
        .filter(|c| c.chars().any(char::is_alphabetic))
        .collect();
    if words.is_empty() {
        0
    } else if words
        .iter()
        .all(|c| c.chars().all(|ch| !ch.is_alphabetic() || ch.is_uppercase()))
    {
        2
    } else {
        u8::from(words.iter().any(|c| is_title_case_word(c)))
    }
}

/// Check if a word is title case
fn is_title_case_word(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }

    let first_char = trimmed.chars().next();
    first_char.is_some_and(char::is_uppercase) && trimmed.chars().skip(1).any(char::is_lowercase)
}

/// Generate markdown table
fn generate_table_markdown(headers: Option<&Vec<String>>, rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let col_count = headers.map_or_else(
        || rows.iter().map(std::vec::Vec::len).max().unwrap_or(0),
        std::vec::Vec::len,
    );

    if col_count == 0 {
        return String::new();
    }

    let mut md = String::new();

    // Header row: Markdown requires one, so a table without detected
    // headers gets an empty header row instead of promoting a data row.
    let blank = vec![String::new(); col_count];
    let hdrs = headers.unwrap_or(&blank);
    md.push('|');
    for i in 0..col_count {
        let h = hdrs.get(i).map_or("", String::as_str);
        md.push_str(if h.is_empty() { " " } else { h });
        if i < col_count - 1 {
            md.push('|');
        }
    }
    md.push_str("|\n");

    // Separator
    md.push('|');
    for i in 0..col_count {
        md.push_str("---");
        if i < col_count - 1 {
            md.push('|');
        }
    }
    md.push_str("|\n");

    // Data rows
    for row in rows {
        md.push('|');
        for i in 0..col_count {
            let cell = row.get(i).map_or("", std::string::String::as_str);
            md.push_str(if cell.is_empty() { " " } else { cell });
            if i < col_count - 1 {
                md.push('|');
            }
        }
        md.push_str("|\n");
    }

    md
}

// ============================================================================
// Heading Detection (improved in v1.2.0)
// ============================================================================

/// Pre-compiled regex patterns for heading detection
struct HeadingPatterns {
    numbered_section: Regex,
    roman_numeral: Regex,
    structural_keyword: Regex,
    academic_keyword: Regex,
    legal_pattern: Regex,
    page_ref_suffix: Regex,
    all_caps: Regex,
}

static HEADING_PATTERNS: LazyLock<HeadingPatterns> = LazyLock::new(|| {
    HeadingPatterns {
    // Matches: "1.", "1.2", "1.2.3", "1.2.3.4" followed by space and text
    numbered_section: Regex::new(r"^(\d+(?:\.\d+)*\.?)\s+([A-Z][A-Za-z].*)").unwrap(),

    // Matches Roman numerals I through XXXIX
    roman_numeral: Regex::new(r"^((?:X{0,3})(?:IX|IV|V?I{0,3}))[\.\)]\s*(.+)?").unwrap(),

    // Structural keywords with number/numeral
    structural_keyword: Regex::new(
        r"(?i)^(Chapter|Section|Part|Article|Schedule|Appendix|Annex|Exhibit)\s+(\d+(?:\.\d+)*|[A-Z]|[IVXLC]+)",
    )
    .unwrap(),

    // Common academic/business section names
    academic_keyword: Regex::new(
        r"(?i)^(Abstract|Introduction|Background|Methods?|Methodology|Materials?\s+and\s+Methods?|Results?|Discussion|Conclusions?|Summary|References|Bibliography|Acknowledgm?ents?|Appendix|Executive\s+Summary|Overview|Scope|Objectives?|Recommendations?|Findings|Analysis|Implementation|Evaluation|Future\s+Work|Related\s+Work|Literature\s+Review|Theoretical\s+Framework|Data\s+Collection|Limitations?|Implications?)\s*:?\s*$",
    )
    .unwrap(),

    // Legal document patterns
    legal_pattern: Regex::new(
        r"(?i)^(WHEREAS|NOW,?\s+THEREFORE|WITNESSETH|RECITALS?|DEFINITIONS?|TERMS?\s+AND\s+CONDITIONS?|REPRESENTATIONS?\s+AND\s+WARRANTIES?|COVENANTS?|INDEMNIFICATION|GOVERNING\s+LAW|MISCELLANEOUS|NOTICES?|AMENDMENTS?|ENTIRE\s+AGREEMENT)\s*:?\s*$",
    )
    .unwrap(),

    // Pattern to strip trailing page references (requires dots or multiple spaces before number)
    page_ref_suffix: Regex::new(r"(?:\.{2,}|·+|\s{3,})\s*\d+\s*$").unwrap(),

    // ALL CAPS with at least 3 alphabetic chars
    all_caps: Regex::new(r"^[A-Z][A-Z0-9\s\-:,&]{2,}$").unwrap(),
}
});

/// Heading detection configuration
struct HeadingConfig {
    min_content_before_heading: usize,
    max_heading_length: usize,
    min_heading_length: usize,
}

impl Default for HeadingConfig {
    fn default() -> Self {
        Self {
            min_content_before_heading: 50,
            max_heading_length: 100,
            min_heading_length: 3,
        }
    }
}

/// Result of heading analysis
struct HeadingMatch {
    text: String,
    level: u8,
    confidence: f32,
}

/// Check if a line is likely a heading
fn detect_heading(line: &str, config: &HeadingConfig) -> Option<HeadingMatch> {
    let trimmed = line.trim();

    // Quick rejections
    if trimmed.len() < config.min_heading_length || trimmed.len() > config.max_heading_length {
        return None;
    }

    // Headings typically don't end with sentence punctuation
    if trimmed.ends_with('.')
        && !trimmed.ends_with("..")
        && !HEADING_PATTERNS.numbered_section.is_match(trimmed)
    {
        return None;
    }
    if trimmed.ends_with(',') || trimmed.ends_with(';') {
        return None;
    }

    // Strip trailing page references
    let cleaned = HEADING_PATTERNS
        .page_ref_suffix
        .replace(trimmed, "")
        .trim()
        .to_string();

    if cleaned.is_empty() {
        return None;
    }

    // Try patterns in order of specificity

    // 1. Structural keywords (highest confidence)
    if HEADING_PATTERNS.structural_keyword.is_match(&cleaned) {
        return Some(HeadingMatch {
            text: cleaned,
            level: 1,
            confidence: 0.95,
        });
    }

    // 2. Academic keywords
    if HEADING_PATTERNS.academic_keyword.is_match(&cleaned) {
        return Some(HeadingMatch {
            text: cleaned.trim_end_matches(':').trim().to_string(),
            level: 1,
            confidence: 0.90,
        });
    }

    // 3. Legal keywords
    if HEADING_PATTERNS.legal_pattern.is_match(&cleaned) {
        return Some(HeadingMatch {
            text: cleaned.trim_end_matches(':').trim().to_string(),
            level: 1,
            confidence: 0.90,
        });
    }

    // 4. Numbered sections
    if let Some(caps) = HEADING_PATTERNS.numbered_section.captures(&cleaned) {
        let number_part = caps.get(1).map_or("", |m| m.as_str());
        // Count dots between numbers (not trailing dots)
        // "1." -> level 1, "1.2" -> level 2, "1.2.3" -> level 3
        let trimmed_number = number_part.trim_end_matches('.');
        let level = trimmed_number.matches('.').count() + 1;
        return Some(HeadingMatch {
            text: cleaned,
            level: u8::try_from(level.min(6)).unwrap_or(6),
            confidence: 0.85,
        });
    }

    // 5. Roman numerals
    if let Some(caps) = HEADING_PATTERNS.roman_numeral.captures(&cleaned) {
        let numeral = caps.get(1).map_or("", |m| m.as_str());
        if is_valid_roman_numeral(numeral) {
            return Some(HeadingMatch {
                text: cleaned,
                level: 1,
                confidence: 0.80,
            });
        }
    }

    // 6. ALL CAPS lines
    if cleaned.len() >= 4 && HEADING_PATTERNS.all_caps.is_match(&cleaned) {
        let alpha_count = cleaned.chars().filter(|c| c.is_alphabetic()).count();
        if alpha_count >= 3 {
            return Some(HeadingMatch {
                text: cleaned,
                level: 1,
                confidence: 0.70,
            });
        }
    }

    // 7. High uppercase ratio (legacy heuristic)
    let upper_count = cleaned.chars().filter(|c| c.is_uppercase()).count();
    let alpha_count = cleaned.chars().filter(|c| c.is_alphabetic()).count();
    if alpha_count > 3 && upper_count > alpha_count / 2 && cleaned.len() < 80 {
        return Some(HeadingMatch {
            text: cleaned,
            level: 2,
            confidence: 0.40,
        });
    }

    None
}

/// Validate Roman numeral string
fn is_valid_roman_numeral(s: &str) -> bool {
    if s.is_empty() || s.len() > 4 {
        return false;
    }
    let valid = Regex::new(r"^M{0,3}(CM|CD|D?C{0,3})(XC|XL|L?X{0,3})(IX|IV|V?I{0,3})$").unwrap();
    valid.is_match(s)
}

// ============================================================================
// Main Extraction Logic
// ============================================================================

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

    // Detect tables before section parsing
    let tables = detect_tables(&text);

    // Parse the extracted text into sections
    let sections = parse_sections(&text);

    // Extract any URLs from the text
    let links = extract_links(&text);

    // Title: the PDF's metadata title, else its first line, else the file name
    let title = info.title.or_else(|| extract_title(&text, name));

    // Calculate total chars
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

/// Extract a PDF's text, keeping wide gaps between text runs as runs of spaces.
///
/// pdf-extract's own plain text output writes one space for any gap, so table
/// cells drawn at separate positions on the page ("Region  Q3  Q4") come out as
/// "Region Q3 Q4" and no column can be found. [`LayoutText`] writes the same
/// text but pads a wide gap to the column where the next run starts.
///
/// Also returns the PDF's metadata, see [`PdfInfo`].
fn extract_layout_text(bytes: &[u8]) -> Result<(String, PdfInfo), pdf_extract::OutputError> {
    let mut doc = pdf_extract::Document::load_mem(bytes)?;
    let mut out = LayoutText::default();
    if doc.is_encrypted() {
        // Many PDFs are encrypted with an empty user password (copy or print
        // restrictions only); pdf-extract's own functions try the same.
        pdf_extract::output_doc_encrypted(&mut doc, &mut out, "")?;
    } else {
        pdf_extract::output_doc(&doc, &mut out)?;
    }
    let info = PdfInfo {
        title: info_string(&doc, b"Title").filter(|t| usable_title(t)),
        author: info_string(&doc, b"Author").filter(|a| usable_author(a)),
    };
    Ok((out.text, info))
}

/// Title and author from the PDF's metadata (the Info dictionary), when usable
#[derive(Debug, Default)]
struct PdfInfo {
    title: Option<String>,
    author: Option<String>,
}

/// A text entry of the PDF's Info dictionary, with whitespace collapsed
fn info_string(doc: &pdf_extract::Document, key: &[u8]) -> Option<String> {
    let raw = doc
        .trailer
        .get_deref(b"Info", doc)
        .and_then(pdf_extract::Object::as_dict)
        .and_then(|info| info.get_deref(key, doc))
        .and_then(pdf_extract::Object::as_str)
        .ok()?;
    let text = decode_pdf_string(raw);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// Decode a PDF text string: UTF-16BE with a byte order mark, else
/// `PDFDocEncoding`, which is close to Latin-1 for the characters titles use
fn decode_pdf_string(raw: &[u8]) -> String {
    match raw {
        [0xFE, 0xFF, rest @ ..] => {
            let units: Vec<u16> = rest
                .chunks_exact(2)
                .map(|p| u16::from_be_bytes([p[0], p[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => raw.iter().map(|&b| char::from(b)).collect(),
    }
}

/// A metadata author worth showing: not a placeholder some PDF writers put
/// there by default ("anonymous", "Administrator")
fn usable_author(author: &str) -> bool {
    let placeholders = [
        "anonymous",
        "unknown",
        "author",
        "user",
        "admin",
        "administrator",
        "owner",
    ];
    author.chars().count() > 1
        && author.chars().count() <= 200
        && !placeholders.contains(&author.to_lowercase().as_str())
}

/// A metadata title worth showing: not "Untitled", not a file name left
/// behind by the program that made the PDF (`report_v3.docx`)
fn usable_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    let file_name = [
        ".pdf", ".doc", ".docx", ".tex", ".dvi", ".ppt", ".pptx", ".xls", ".xlsx", ".indd", ".odt",
        ".rtf", ".txt", ".html",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext));
    title.chars().count() > 3
        && title.chars().count() <= 200
        && !file_name
        && !lower.starts_with("untitled")
        && !lower.starts_with("microsoft word -")
}

/// A gap at least this wide (in font sizes) separates columns, not words.
/// Word spaces are about 0.25 em, and stay under 0.7 em even in justified text.
const COLUMN_GAP_EM: f64 = 1.0;
/// Estimated width of one character (in font sizes), to turn page positions
/// into character columns. Only alignment between rows matters, not accuracy.
const CHAR_WIDTH_EM: f64 = 0.5;

/// Plain text output that follows pdf-extract's `PlainTextOutput` (same line
/// breaks and word spaces) but keeps column gaps, see [`extract_layout_text`].
#[derive(Default)]
struct LayoutText {
    text: String,
    page_height: f64,
    /// x where the last character ended
    last_end: f64,
    last_y: f64,
    /// true at the first character of a word, where spacing is decided
    first_char: bool,
    /// x where the current line's first character starts
    line_origin: Option<f64>,
    /// characters written on the current line
    line_chars: usize,
}

impl LayoutText {
    fn new_line(&mut self) {
        self.text.push('\n');
        self.line_origin = None;
        self.line_chars = 0;
    }

    fn push(&mut self, s: &str) {
        self.text.push_str(s);
        self.line_chars += s.chars().count();
    }

    /// Spaces to write for a gap that starts a new text run at `x`
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn gap_spaces(&self, x: f64, font_size: f64) -> usize {
        let gap = x - self.last_end;
        match self.line_origin {
            Some(origin) if gap >= font_size * COLUMN_GAP_EM && font_size > 0.0 => {
                // Pad to the column of `x`, so cells in the same column line
                // up across rows; at least two spaces mark a column gap.
                let column = ((x - origin) / (font_size * CHAR_WIDTH_EM)).round();
                let column = if column.is_finite() && column > 0.0 {
                    column.min(10_000.0) as usize
                } else {
                    0
                };
                column.saturating_sub(self.line_chars).max(2)
            }
            _ => 1,
        }
    }
}

impl pdf_extract::OutputDev for LayoutText {
    fn begin_page(
        &mut self,
        _page_num: u32,
        media_box: &pdf_extract::MediaBox,
        _art_box: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), pdf_extract::OutputError> {
        self.page_height = media_box.ury - media_box.lly;
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), pdf_extract::OutputError> {
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &pdf_extract::Transform,
        width: f64,
        _spacing: f64,
        font_size: f64,
        char: &str,
    ) -> Result<(), pdf_extract::OutputError> {
        // Same geometry as PlainTextOutput: flip y so it grows down the page,
        // and the font size as transformed by the text matrix.
        let (x, y) = (trm.m31, self.page_height - trm.m32);
        let size_x = font_size * (trm.m11 + trm.m21);
        let size_y = font_size * (trm.m12 + trm.m22);
        let size = (size_x * size_y).abs().sqrt();

        if self.first_char {
            let mut broke = false;
            if (y - self.last_y).abs() > size * 1.5 {
                self.new_line();
                broke = true;
            }
            // moved left and down: a new line
            if x < self.last_end && (y - self.last_y).abs() > size * 0.5 {
                self.new_line();
                broke = true;
            }
            if !broke && x > self.last_end + size * 0.1 {
                let spaces = self.gap_spaces(x, size);
                self.push(&" ".repeat(spaces));
            }
        }
        if self.line_origin.is_none() {
            self.line_origin = Some(x);
        }
        self.push(char);
        self.first_char = false;
        self.last_y = y;
        self.last_end = x + width * size;
        Ok(())
    }

    fn begin_word(&mut self) -> Result<(), pdf_extract::OutputError> {
        self.first_char = true;
        Ok(())
    }

    fn end_word(&mut self) -> Result<(), pdf_extract::OutputError> {
        Ok(())
    }

    fn end_line(&mut self) -> Result<(), pdf_extract::OutputError> {
        Ok(())
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

/// Maximum characters per PDF section
const PDF_SECTION_CHARS: usize = 10_000;
/// Maximum sections per PDF
const PDF_MAX_SECTIONS: usize = 100;

fn parse_sections(text: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let config = HeadingConfig::default();

    let mut current_heading = "Content".to_string();
    let mut current_content = String::new();
    let mut current_level: u8 = 1;
    let mut first_heading_pending = true;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !current_content.is_empty() {
                current_content.push_str("\n\n");
            }
            continue;
        }

        // Check if this looks like a heading. A line with two or more column
        // gaps is a table row, not a heading ("2025   Total   300").
        let heading_match = if find_gaps(trimmed, 2).len() >= 2 {
            None
        } else {
            detect_heading(trimmed, &config).map(|mut hm| {
                hm.text = hm.text.split_whitespace().collect::<Vec<_>>().join(" ");
                hm
            })
        };

        let is_heading = heading_match.as_ref().is_some_and(|h| h.confidence >= 0.5);

        // A heading before any text names the first section instead of the
        // "Content" placeholder (so "1. Introduction" on the first line is a heading)
        if is_heading && sections.is_empty() && first_heading_pending && current_content.is_empty()
        {
            if let Some(hm) = heading_match {
                current_heading = hm.text;
                current_level = hm.level;
            }
            first_heading_pending = false;
            continue;
        }
        first_heading_pending = false;

        // A confident heading (numbered, Roman numeral, keyword) starts a new
        // section after any text; a weak one (ALL CAPS, mostly uppercase) only
        // after enough text, so short runs of capitals do not split sections.
        let confident = heading_match.as_ref().is_some_and(|h| h.confidence >= 0.8);

        if is_heading
            && !current_content.is_empty()
            && (confident || current_content.len() >= config.min_content_before_heading)
        {
            // Save current section
            sections.push(Section {
                level: current_level,
                heading: truncate(&current_heading, 200),
                content: truncate(current_content.trim(), PDF_SECTION_CHARS),
                truncated: current_content.trim().len() > PDF_SECTION_CHARS,
            });

            // Start new section
            let hm = heading_match.unwrap();
            current_heading = hm.text;
            current_level = hm.level;
            current_content = String::new();
        } else {
            if !current_content.is_empty() && !current_content.ends_with('\n') {
                current_content.push(' ');
            }
            // Column gaps are kept for table detection; in section text one
            // space is enough (the `tables` field keeps the layout)
            let mut words = trimmed.split_whitespace();
            if let Some(first) = words.next() {
                current_content.push_str(first);
                for word in words {
                    current_content.push(' ');
                    current_content.push_str(word);
                }
            }
        }
    }

    // Add final section
    if !current_content.is_empty() {
        sections.push(Section {
            level: current_level,
            heading: truncate(&current_heading, 200),
            content: truncate(current_content.trim(), PDF_SECTION_CHARS),
            truncated: current_content.trim().len() > PDF_SECTION_CHARS,
        });
    }

    sections.truncate(PDF_MAX_SECTIONS);
    sections
}

fn extract_title(text: &str, path: &Path) -> Option<String> {
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

fn extract_author(text: &str) -> Option<String> {
    // A labelled line ("Author: Jane Doe", "Written by Jane Doe"), or a line
    // that is only "By" and a list of names ("By Jane Doe, Ana Lima").
    // "by region, in thousands" inside a sentence is neither. Only the
    // keywords ignore case: names must start with a capital.
    let author_re = Regex::new(
        r"(?m)^[ \t]*(?:(?i:authors?|written by)[ \t]*:?[ \t]+(\p{Lu}[^\n]{0,150})|(?i:by)[ \t]+(\p{Lu}[\p{L}.' -]{1,60}(?:,[ \t]*\p{Lu}[\p{L}.' -]{1,60})*))[ \t]*$",
    )
    .ok()?;
    author_re
        .captures(text)
        .and_then(|c| c.get(1).or_else(|| c.get(2)))
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| s.len() < 200)
}

fn extract_date(text: &str) -> Option<String> {
    let date_re = Regex::new(
        r"(?i)(?:date|published|updated)[:\s]+(\d{4}[-/]\d{2}[-/]\d{2}|\w+\s+\d{1,2},?\s+\d{4})",
    )
    .ok()?;
    date_re.captures(text).map(|c| c[1].trim().to_string())
}

fn extract_doi(text: &str) -> Option<String> {
    let doi_re = Regex::new(r"(?i)(?:doi[:\s]+|https?://doi\.org/)(10\.\d{4,}/[^\s\)]+)").ok()?;
    doi_re.captures(text).map(|c| c[1].trim().to_string())
}

fn extract_links(text: &str) -> Vec<Link> {
    let url_re = Regex::new(r#"https?://[^\s\)>\]"']+"#).unwrap();
    let mut links = Vec::new();
    let mut seen = HashSet::new();

    for mat in url_re.find_iter(text) {
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

fn extract_code(text: &str) -> Vec<CodeBlock> {
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Table Detection Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_find_gaps() {
        let line = "Name      Age    City";
        let gaps = find_gaps(line, 2);
        assert_ne!(gaps, Vec::<usize>::new());
        assert!(gaps.len() >= 2);
    }

    #[test]
    fn test_is_potential_table_line() {
        let config = TableConfig::default();

        // Table-like lines
        assert!(is_potential_table_line("Name      Age    City", &config));
        assert!(is_potential_table_line("Revenue   2024   2025", &config));
        assert!(is_potential_table_line("$10M      $15M   $20M", &config));

        // Not table lines
        assert!(!is_potential_table_line(
            "This is a regular sentence.",
            &config
        ));
        assert!(!is_potential_table_line("Short", &config));
        assert!(!is_potential_table_line(
            "The quick brown fox jumps over the lazy dog.",
            &config
        ));
    }

    #[test]
    fn test_is_prose_line() {
        assert!(is_prose_line(
            "The company reported strong earnings this quarter."
        ));
        assert!(is_prose_line(
            "This is a longer sentence with articles and typical prose patterns."
        ));

        assert!(!is_prose_line("Revenue   2024   2025"));
        assert!(!is_prose_line("$10M"));
    }

    #[test]
    fn test_detect_tables_simple() {
        let text = r"
Financial Summary

Item        2024      2025
Revenue     $10M      $15M
Costs       $5M       $7M
Profit      $5M       $8M

This is a paragraph of text that follows the table.
";

        let tables = detect_tables(text);
        assert!(!tables.is_empty(), "Should detect at least one table");

        let table = &tables[0];
        assert!(
            table.rows.len() >= 3,
            "Table should have at least 3 rows of data"
        );
        assert!(!table.markdown.is_empty(), "Should generate markdown");
    }

    #[test]
    fn test_detect_tables_with_header() {
        let text = r"
Name          Age    City
John          25     NYC
Jane          30     LA
Bob           35     Chicago
";

        let tables = detect_tables(text);
        assert!(!tables.is_empty());

        let table = &tables[0];
        assert!(
            table.headers.is_some(),
            "Should detect header row (Name, Age, City)"
        );
    }

    // ---- PDF layout: tables, headings, metadata (tests/fixtures/report.pdf) ----

    /// The fixture made by `tests/fixtures/make_report_pdf.py`: a first-line
    /// heading, a table drawn cell by cell with right-aligned numbers and
    /// spaced-out rows, and a heading drawn as number + title.
    fn report_fixture() -> PdfPage {
        let bytes = include_bytes!("../tests/fixtures/report.pdf");
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
        let bytes = include_bytes!("../tests/fixtures/signed-in.pdf");
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
    fn cell_runs_split_on_two_or_more_spaces() {
        assert_eq!(
            cell_runs("  Warm Sand    150   7 %  "),
            ["Warm Sand", "150", "7 %"]
        );
        assert_eq!(cell_runs("one cell"), ["one cell"]);
        assert!(cell_runs("   ").is_empty());
    }

    #[test]
    fn blank_lines_between_rows_do_not_end_a_table() {
        let text = "Region   Q3   Q4\n\nNorth   150   175\n\nSouth   200   210\n";
        let tables = detect_tables(text);
        assert_eq!(tables.len(), 1, "{tables:?}");
        assert_eq!(tables[0].rows.len(), 2);
        assert!(tables[0].headers.is_some());
    }

    #[test]
    fn a_row_with_other_cells_after_a_blank_line_starts_a_new_table() {
        let text = "Alpha   1   2\nBeta   3   4\n\nGamma   5\nDelta   6\n";
        let tables = detect_tables(text);
        assert_eq!(tables.len(), 2, "{tables:?}");
        assert_eq!(tables[0].rows[0].len(), 3);
        assert_eq!(tables[1].rows[0].len(), 2);
    }

    #[test]
    fn a_heading_line_is_not_a_table_row() {
        let text = "North   140   150\nSouth   190   200\n\n2.1  Notes on the data\n";
        let tables = detect_tables(text);
        assert_eq!(tables.len(), 1);
        assert!(
            tables[0]
                .rows
                .iter()
                .flatten()
                .all(|c| !c.contains("Notes")),
            "{tables:?}"
        );
    }

    #[test]
    fn rows_styled_alike_have_no_header() {
        // Colour codes and names: the first row is data like the rest
        let text = "#1a2332   Teal\n#2d8b8b   Seafoam\n#a8dadc   Cream\n";
        let tables = detect_tables(text);
        assert_eq!(tables.len(), 1);
        assert!(tables[0].headers.is_none(), "{tables:?}");
        assert_eq!(tables[0].rows.len(), 3);
        // ALL CAPS over Title Case is a header
        let caps = detect_tables("NAME   ROLE\nAlice   Engineer\nBruno   Designer\n");
        assert_eq!(
            caps[0].headers.as_deref(),
            Some(&["NAME", "ROLE"].map(String::from)[..])
        );
    }

    #[test]
    fn first_line_heading_names_the_first_section() {
        let sections = parse_sections("1. Introduction\n\nShort text.\n");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "1. Introduction");
        assert_eq!(sections[0].content, "Short text.");
    }

    #[test]
    fn confident_headings_split_short_sections_but_weak_ones_do_not() {
        // Numbered headings split even after a short title line
        let sections =
            parse_sections("Quarterly Report\n1. Introduction\nText.\n2. Results\nMore.\n");
        let headings: Vec<&str> = sections.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(headings, ["Content", "1. Introduction", "2. Results"]);
        // An ALL CAPS line after a few words stays in the text
        let sections = parse_sections("Intro words.\nNOTE WELL\nmore text\n");
        assert_eq!(sections.len(), 1);
        assert!(sections[0].content.contains("NOTE WELL"));
    }

    #[test]
    fn table_rows_are_not_headings_and_heading_gaps_collapse() {
        let text = "Opening text that is long enough to stand on its own here.\n\
                    2025   Total   300\n\
                    2  Results\n\
                    Body.\n";
        let sections = parse_sections(text);
        let headings: Vec<&str> = sections.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(headings, ["Content", "2 Results"]);
        assert!(sections[0].content.contains("2025 Total 300"));
    }

    #[test]
    fn layout_gap_spaces_line_up_columns() {
        let mut out = LayoutText {
            line_origin: Some(72.0),
            line_chars: 6, // "Region"
            last_end: 110.0,
            ..LayoutText::default()
        };
        // Next run starts 150 pt right of the line start at font size 10:
        // column 30 at 5 pt per character
        assert_eq!(out.gap_spaces(222.0, 10.0), 24);
        // A normal word space stays one space
        assert_eq!(out.gap_spaces(113.0, 10.0), 1);
        // Text already past the column still gets a two-space column gap
        out.line_chars = 40;
        assert_eq!(out.gap_spaces(222.0, 10.0), 2);
        // No line start known (first run of a line): one space
        out.line_origin = None;
        assert_eq!(out.gap_spaces(222.0, 10.0), 1);
    }

    #[test]
    fn pdf_strings_and_metadata_filters() {
        assert_eq!(
            decode_pdf_string(&[0xFE, 0xFF, 0x00, 0x52, 0x00, 0xE9]),
            "Ré"
        );
        assert_eq!(decode_pdf_string(b"Caf\xe9"), "Café");
        assert!(usable_title("Annual Report 2026"));
        assert!(!usable_title("report_v3.docx"));
        assert!(!usable_title("Microsoft Word - plan"));
        assert!(!usable_title("Untitled document"));
        assert!(!usable_title("abc"));
        assert!(usable_author("Jane Doe"));
        assert!(!usable_author("anonymous"));
        assert!(!usable_author("Administrator"));
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
    }

    #[test]
    fn test_has_numeric_value() {
        assert!(has_numeric_value("$10M"));
        assert!(has_numeric_value("50%"));
        assert!(has_numeric_value("123"));
        assert!(has_numeric_value("(100)"));

        assert!(has_numeric_value("1,234.5"));
        assert!(has_numeric_value(".5"));
        assert!(has_numeric_value("-12%"));

        assert!(!has_numeric_value("Name"));
        assert!(!has_numeric_value("Revenue"));
        assert!(!has_numeric_value("Q3"));
        assert!(!has_numeric_value("FY2026"));
        assert!(!has_numeric_value(""));
    }

    #[test]
    fn test_generate_table_markdown() {
        let headers = Some(vec![
            "Name".to_string(),
            "Age".to_string(),
            "City".to_string(),
        ]);
        let rows = vec![
            vec!["John".to_string(), "25".to_string(), "NYC".to_string()],
            vec!["Jane".to_string(), "30".to_string(), "LA".to_string()],
        ];

        let md = generate_table_markdown(headers.as_ref(), &rows);

        assert!(md.contains("|Name|Age|City|"));
        assert!(md.contains("|---|---|---|"));
        assert!(md.contains("|John|25|NYC|"));
    }

    #[test]
    fn test_generate_table_markdown_without_headers_is_valid() {
        let rows = vec![
            vec!["North".to_string(), "120".to_string()],
            vec!["South".to_string(), "90".to_string()],
        ];
        let md = generate_table_markdown(None, &rows);
        assert!(md.starts_with("| | |\n|---|---|\n|North|120|"), "{md}");
    }

    // -------------------------------------------------------------------------
    // Heading Detection Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_numbered_sections() {
        let config = HeadingConfig::default();

        let h = detect_heading("1. Introduction", &config).unwrap();
        assert_eq!(h.level, 1);
        assert!(h.confidence >= 0.80);

        let h = detect_heading("1.2 Methods", &config).unwrap();
        assert_eq!(h.level, 2);

        let h = detect_heading("1.2.3 Data Analysis", &config).unwrap();
        assert_eq!(h.level, 3);

        let h = detect_heading("2.1.4.2 Statistical Methods", &config).unwrap();
        assert_eq!(h.level, 4);
    }

    #[test]
    fn test_roman_numerals() {
        let config = HeadingConfig::default();

        let h = detect_heading("I. Introduction", &config).unwrap();
        assert!(h.confidence >= 0.70);

        let h = detect_heading("IV. Results", &config).unwrap();
        assert!(h.text.contains("Results"));

        let h = detect_heading("II. Background", &config).unwrap();
        assert!(h.text.contains("Background"));
    }

    #[test]
    fn test_academic_keywords() {
        let config = HeadingConfig::default();

        assert!(detect_heading("Abstract", &config).is_some());
        assert!(detect_heading("Introduction", &config).is_some());
        assert!(detect_heading("Methods", &config).is_some());
        assert!(detect_heading("Results", &config).is_some());
        assert!(detect_heading("Discussion", &config).is_some());
        assert!(detect_heading("Conclusion", &config).is_some());
        assert!(detect_heading("References", &config).is_some());
        assert!(detect_heading("Acknowledgments", &config).is_some());
        assert!(detect_heading("Literature Review", &config).is_some());
        assert!(detect_heading("Executive Summary", &config).is_some());
    }

    #[test]
    fn test_structural_keywords() {
        let config = HeadingConfig::default();

        let h = detect_heading("Chapter 3", &config).unwrap();
        assert!(h.confidence >= 0.90);

        let h = detect_heading("Section 2.1", &config).unwrap();
        assert!(h.confidence >= 0.90);

        let h = detect_heading("Appendix A", &config).unwrap();
        assert!(h.confidence >= 0.90);

        let h = detect_heading("Article IV", &config).unwrap();
        assert!(h.confidence >= 0.90);
    }

    #[test]
    fn test_legal_patterns() {
        let config = HeadingConfig::default();

        assert!(detect_heading("WHEREAS", &config).is_some());
        assert!(detect_heading("DEFINITIONS", &config).is_some());
        assert!(detect_heading("TERMS AND CONDITIONS", &config).is_some());
        assert!(detect_heading("GOVERNING LAW", &config).is_some());
    }

    #[test]
    fn test_all_caps_headings() {
        let config = HeadingConfig::default();

        let h = detect_heading("FINANCIAL SUMMARY", &config).unwrap();
        assert!(h.confidence >= 0.60);

        let h = detect_heading("QUARTERLY RESULTS", &config).unwrap();
        assert!(h.confidence >= 0.60);
    }

    #[test]
    fn test_trailing_page_refs_stripped() {
        let config = HeadingConfig::default();

        let h = detect_heading("Introduction .......... 15", &config).unwrap();
        assert!(!h.text.contains("15"));
        assert!(!h.text.contains(".."));

        let h = detect_heading("Methods   42", &config).unwrap();
        assert!(!h.text.ends_with("42"));
    }

    #[test]
    fn test_not_headings() {
        let config = HeadingConfig::default();

        // Regular sentences
        assert!(detect_heading("This is a regular sentence.", &config).is_none());

        // Too short
        assert!(detect_heading("Hi", &config).is_none());

        // Too long
        let long = "A".repeat(150);
        assert!(detect_heading(&long, &config).is_none());

        // Ends with comma
        assert!(detect_heading("First item,", &config).is_none());

        // Ends with semicolon
        assert!(detect_heading("Some text;", &config).is_none());
    }

    #[test]
    fn test_is_valid_roman_numeral() {
        assert!(is_valid_roman_numeral("I"));
        assert!(is_valid_roman_numeral("IV"));
        assert!(is_valid_roman_numeral("IX"));
        assert!(is_valid_roman_numeral("X"));
        assert!(is_valid_roman_numeral("XIV"));

        assert!(!is_valid_roman_numeral(""));
        assert!(!is_valid_roman_numeral("IIII")); // Invalid
        assert!(!is_valid_roman_numeral("ABC"));
    }

    // -------------------------------------------------------------------------
    // Existing Tests (preserved)
    // -------------------------------------------------------------------------

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

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello...");
    }

    // -------------------------------------------------------------------------
    // URL Detection Tests (v1.6.0)
    // -------------------------------------------------------------------------

    #[test]
    fn test_is_url() {
        assert!(is_url("https://example.com/doc.pdf"));
        assert!(is_url("http://example.com/doc.pdf"));
        assert!(!is_url("/local/path/doc.pdf"));
        assert!(!is_url("relative/path.pdf"));
        assert!(!is_url("C:\\Windows\\file.pdf"));
    }
}
