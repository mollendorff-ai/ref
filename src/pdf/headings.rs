//! Heading detection in PDF text: numbered sections, Roman numerals,
//! structural, academic and legal keywords, ALL CAPS.

use regex::Regex;
use std::sync::LazyLock;

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
    numbered_section: Regex::new(r"^(\d+(?:\.\d+)*\.?)\s+([A-Z][A-Za-z].*)").expect("constant regex"),

    roman_numeral: Regex::new(r"^((?:X{0,3})(?:IX|IV|V?I{0,3}))[\.\)]\s*(.+)?").expect("constant regex"),

    // Structural keywords with number/numeral
    structural_keyword: Regex::new(
        r"(?i)^(Chapter|Section|Part|Article|Schedule|Appendix|Annex|Exhibit)\s+(\d+(?:\.\d+)*|[A-Z]|[IVXLC]+)",
    )
    .expect("constant regex"),

    // Common academic/business section names
    academic_keyword: Regex::new(
        r"(?i)^(Abstract|Introduction|Background|Methods?|Methodology|Materials?\s+and\s+Methods?|Results?|Discussion|Conclusions?|Summary|References|Bibliography|Acknowledgm?ents?|Appendix|Executive\s+Summary|Overview|Scope|Objectives?|Recommendations?|Findings|Analysis|Implementation|Evaluation|Future\s+Work|Related\s+Work|Literature\s+Review|Theoretical\s+Framework|Data\s+Collection|Limitations?|Implications?)\s*:?\s*$",
    )
    .expect("constant regex"),

    legal_pattern: Regex::new(
        r"(?i)^(WHEREAS|NOW,?\s+THEREFORE|WITNESSETH|RECITALS?|DEFINITIONS?|TERMS?\s+AND\s+CONDITIONS?|REPRESENTATIONS?\s+AND\s+WARRANTIES?|COVENANTS?|INDEMNIFICATION|GOVERNING\s+LAW|MISCELLANEOUS|NOTICES?|AMENDMENTS?|ENTIRE\s+AGREEMENT)\s*:?\s*$",
    )
    .expect("constant regex"),

    page_ref_suffix: Regex::new(r"(?:\.{2,}|·+|\s{3,})\s*\d+\s*$").expect("constant regex"),

    all_caps: Regex::new(r"^[A-Z][A-Z0-9\s\-:,&]{2,}$").expect("constant regex"),
}
});

/// Heading detection configuration
pub(super) struct HeadingConfig {
    pub(super) min_content_before_heading: usize,
    pub(super) max_heading_length: usize,
    pub(super) min_heading_length: usize,
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
pub(super) struct HeadingMatch {
    pub(super) text: String,
    pub(super) level: u8,
    pub(super) confidence: f32,
}

/// Check if a line is likely a heading
pub(super) fn detect_heading(line: &str, config: &HeadingConfig) -> Option<HeadingMatch> {
    let trimmed = line.trim();

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

    let cleaned = HEADING_PATTERNS
        .page_ref_suffix
        .replace(trimmed, "")
        .trim()
        .to_string();

    if cleaned.is_empty() {
        return None;
    }

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
    ROMAN_RE.is_match(s)
}

/// A valid Roman numeral (up to MMMCMXCIX)
static ROMAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^M{0,3}(CM|CD|D?C{0,3})(XC|XL|L?X{0,3})(IX|IV|V?I{0,3})$").expect("constant regex")
});

#[cfg(test)]
mod tests {
    use super::*;

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

    // Existing Tests (preserved)
}
