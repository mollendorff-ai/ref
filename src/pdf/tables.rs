//! Table detection in PDF text: columns from whitespace, header rows, Markdown.

use super::headings::{detect_heading, HeadingConfig};
use serde::Serialize;

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
pub(super) fn detect_tables(text: &str) -> Vec<Table> {
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

    if trimmed.len() < 5 {
        return false;
    }

    if is_prose_line(trimmed) {
        return false;
    }

    // Count significant whitespace gaps (2+ spaces)
    let gaps = find_gaps(line, config.min_gap_chars);

    gaps.len() >= config.min_columns - 1
}

/// Find positions of whitespace gaps in a line
pub(super) fn find_gaps(line: &str, min_gap: usize) -> Vec<usize> {
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

    if trimmed.len() > 150 {
        return true;
    }

    if trimmed.ends_with('.') && !trimmed.ends_with("..") {
        let word_count = trimmed.split_whitespace().count();
        if word_count >= 6 {
            return true;
        }
    }

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

    let (headers, data_rows) = detect_header_row(&rows);

    let markdown = generate_table_markdown(headers.as_ref(), &data_rows);

    Some(Table {
        headers,
        rows: data_rows,
        markdown,
    })
}

/// Find consistent column separator positions across lines
fn find_column_positions(lines: &[&str], config: &TableConfig) -> Option<Vec<usize>> {
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

    let mut position_counts: Vec<(usize, usize)> = Vec::new(); // (position, count)

    for gaps in &all_gaps {
        for &gap_pos in gaps {
            let mut found = false;
            for (pos, count) in &mut position_counts {
                if pos.abs_diff(gap_pos) <= config.column_tolerance {
                    *count += 1;
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

    let mut header_score = 0;

    let first_has_numbers = first_row.iter().any(|c| has_numeric_value(c));
    let second_has_numbers = second_row.iter().any(|c| has_numeric_value(c));

    if !first_has_numbers && second_has_numbers {
        header_score += 3;
    }

    // Compare averages using integer cross-multiplication to avoid float casts:
    // first_avg < second_avg * 0.8  =>  first_total * second_count * 10 < second_total * first_count * 8
    let first_total: usize = first_row.iter().map(String::len).sum();
    let second_total: usize = second_row.iter().map(String::len).sum();
    let first_count = first_row.len().max(1);
    let second_count = second_row.len().max(1);

    if first_total * second_count * 10 < second_total * first_count * 8 {
        header_score += 1;
    }

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

    md.push('|');
    for i in 0..col_count {
        md.push_str("---");
        if i < col_count - 1 {
            md.push('|');
        }
    }
    md.push_str("|\n");

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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn cell_runs_split_on_two_or_more_spaces() {
        assert_eq!(
            cell_runs("  Warm Sand    150   7 %  "),
            ["Warm Sand", "150", "7 %"]
        );
        assert_eq!(cell_runs("one cell"), ["one cell"]);
        assert_eq!(cell_runs("   "), Vec::<String>::new());
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
}
