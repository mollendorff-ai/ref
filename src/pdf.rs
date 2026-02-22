//! pdf command: Extract text from PDF files
//!
//! Local extraction, no external APIs.
//! Output matches fetch command structure for consistency.
//!
//! v1.2.0: Table detection and improved heading detection

use crate::fetch::{CodeBlock, Link, Page, PageStatus, Section};
use anyhow::Result;
use clap::Args;
use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Args)]
pub struct PdfArgs {
    /// PDF files to extract
    #[arg(required = true)]
    pub files: Vec<PathBuf>,
}

/// Run the PDF extraction command.
///
/// # Errors
///
/// Returns an error if JSON serialization fails.
pub fn run_pdf(args: &PdfArgs) -> Result<()> {
    let file_count = args.files.len();
    eprintln!(
        "Extracting {} PDF{}...",
        file_count,
        if file_count == 1 { "" } else { "s" }
    );

    let mut results: Vec<PdfPage> = Vec::new();

    for file in &args.files {
        eprintln!("  -> {}", file.display());
        let page = extract_pdf(file);
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
    Ok(())
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

/// Detect tables in text using whitespace column analysis
fn detect_tables(text: &str) -> Vec<Table> {
    let config = TableConfig::default();
    let lines: Vec<&str> = text.lines().collect();
    let mut tables = Vec::new();
    let mut table_start: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        let is_table_line = is_potential_table_line(line, &config);

        match (table_start, is_table_line) {
            (None, true) => {
                table_start = Some(i);
            }
            (Some(start), false) => {
                // End of table region
                if i - start >= config.min_rows {
                    if let Some(table) = parse_table_region(&lines[start..i], &config) {
                        tables.push(table);
                    }
                }
                table_start = None;
            }
            _ => {}
        }
    }

    // Handle table at end of text
    if let Some(start) = table_start {
        if lines.len() - start >= config.min_rows {
            if let Some(table) = parse_table_region(&lines[start..], &config) {
                tables.push(table);
            }
        }
    }

    // Limit tables
    tables.truncate(20);
    tables
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

    // Find consistent column boundaries across all lines
    let column_positions = find_column_positions(lines, config)?;

    if column_positions.len() < config.min_columns - 1 {
        return None;
    }

    // Extract cells using column positions
    let mut rows: Vec<Vec<String>> = Vec::new();

    for line in lines {
        let cells = extract_cells(line, &column_positions);
        if cells.iter().any(|c| !c.is_empty()) {
            rows.push(cells);
        }
    }

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

    // Check if first row has title case or ALL CAPS
    let first_has_caps = first_row.iter().filter(|c| !c.is_empty()).any(|c| {
        is_title_case_word(c) || c.chars().all(|ch| !ch.is_alphabetic() || ch.is_uppercase())
    });

    if first_has_caps {
        header_score += 2;
    }

    if header_score >= 3 {
        (Some(first_row.clone()), rows[1..].to_vec())
    } else {
        (None, rows.to_vec())
    }
}

/// Check if a string contains numeric values (currency, percentages, etc.)
fn has_numeric_value(s: &str) -> bool {
    let cleaned: String = s
        .chars()
        .filter(|c| !['$', '€', '£', ',', '%', '(', ')', '-', '+', ' '].contains(c))
        .collect();

    !cleaned.is_empty() && cleaned.chars().any(|c| c.is_ascii_digit())
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

    // Header row
    if let Some(hdrs) = headers {
        md.push('|');
        for (i, h) in hdrs.iter().enumerate() {
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
    }

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

pub(crate) fn extract_pdf(path: &PathBuf) -> PdfPage {
    let file_url = format!("file://{}", path.display());

    // Check file exists
    if !path.exists() {
        return error_page(&file_url, "File not found");
    }

    // Extract text using pdf-extract
    let text = match pdf_extract::extract_text(path) {
        Ok(t) => t,
        Err(e) => {
            return error_page(&file_url, &format!("PDF extraction failed: {e}"));
        }
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

    // Try to extract title from first line or filename
    let title = extract_title(&text, path);

    // Calculate total chars
    let chars: usize = sections
        .iter()
        .map(|s| s.content.len() + s.heading.len())
        .sum();

    PdfPage {
        base: Page {
            url: file_url,
            status: PageStatus::Ok,
            title,
            site: None,
            author: extract_author(&text),
            date: extract_date(&text),
            doi: extract_doi(&text),
            sections,
            links,
            code: extract_code(&text),
            alerts: vec![],
            chars,
        },
        tables,
    }
}

fn error_page(url: &str, error: &str) -> PdfPage {
    PdfPage {
        base: Page {
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
        },
        tables: vec![],
    }
}

fn parse_sections(text: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let config = HeadingConfig::default();

    let mut current_heading = "Content".to_string();
    let mut current_content = String::new();
    let mut current_level: u8 = 1;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !current_content.is_empty() {
                current_content.push_str("\n\n");
            }
            continue;
        }

        // Check if this looks like a heading
        let heading_match = detect_heading(trimmed, &config);

        let is_heading = heading_match.as_ref().is_some_and(|h| h.confidence >= 0.5);

        if is_heading && current_content.len() >= config.min_content_before_heading {
            // Save current section
            sections.push(Section {
                level: current_level,
                heading: truncate(&current_heading, 200),
                content: truncate(current_content.trim(), 10000),
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
            current_content.push_str(trimmed);
        }
    }

    // Add final section
    if !current_content.is_empty() {
        sections.push(Section {
            level: current_level,
            heading: truncate(&current_heading, 200),
            content: truncate(current_content.trim(), 10000),
        });
    }

    sections.truncate(100);
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
    let author_re = Regex::new(r"(?i)(?:author|by|written by)[:\s]+([^\n]+)").ok()?;
    author_re
        .captures(text)
        .map(|c| c[1].trim().to_string())
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
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
        assert!(!gaps.is_empty());
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

    #[test]
    fn test_has_numeric_value() {
        assert!(has_numeric_value("$10M"));
        assert!(has_numeric_value("50%"));
        assert!(has_numeric_value("123"));
        assert!(has_numeric_value("(100)"));

        assert!(!has_numeric_value("Name"));
        assert!(!has_numeric_value("Revenue"));
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
}
