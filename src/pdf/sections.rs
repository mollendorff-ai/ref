//! Split PDF text into sections at its headings.

use super::headings::{detect_heading, HeadingConfig};
use super::tables::find_gaps;
use crate::fetch::Section;
use crate::text::truncate;

/// Maximum characters per PDF section
pub(super) const PDF_SECTION_CHARS: usize = 10_000;

/// Maximum sections per PDF
pub(super) const PDF_MAX_SECTIONS: usize = 100;

pub(super) fn parse_sections(text: &str) -> Vec<Section> {
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

        let splits = is_heading
            && !current_content.is_empty()
            && (confident || current_content.len() >= config.min_content_before_heading);

        if let (true, Some(hm)) = (splits, heading_match) {
            sections.push(Section {
                level: current_level,
                heading: truncate(&current_heading, 200),
                content: truncate(current_content.trim(), PDF_SECTION_CHARS),
                truncated: current_content.trim().len() > PDF_SECTION_CHARS,
            });

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
