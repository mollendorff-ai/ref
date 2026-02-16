//! URL extraction from markdown and text content

use regex::Regex;
use std::collections::HashSet;

/// Extract unique URLs from text content.
///
/// # Panics
///
/// Panics if the internal URL regex fails to compile (should never happen).
#[must_use]
pub fn extract_urls(content: &str) -> Vec<String> {
    let re = Regex::new(r#"https?://[^\s\)>\]"'`]+"#).unwrap();

    let mut seen = HashSet::new();
    let mut urls = Vec::new();

    for mat in re.find_iter(content) {
        let url = mat.as_str();
        // Clean trailing punctuation
        let url = url.trim_end_matches([',', '.', ')', ']', ';', ':']);

        if !seen.contains(url) {
            seen.insert(url.to_string());
            urls.push(url.to_string());
        }
    }

    urls
}

/// Extract dollar amounts from text.
///
/// # Panics
///
/// Panics if the internal amount regex fails to compile (should never happen).
#[must_use]
pub fn extract_amounts(text: &str) -> Vec<AmountMatch> {
    let re = Regex::new(r"\$([0-9,.]+)\s*(billion|million|B|M|K)?").unwrap();

    re.captures_iter(text)
        .take(10)
        .map(|cap| AmountMatch {
            value: cap[1].to_string(),
            unit: cap.get(2).map(|m| m.as_str().to_string()),
            raw: cap[0].to_string(),
        })
        .collect()
}

/// Extract percentages from text.
///
/// # Panics
///
/// Panics if the internal percentage regex fails to compile (should never happen).
#[must_use]
pub fn extract_percentages(text: &str) -> Vec<String> {
    let re = Regex::new(r"([0-9,.]+)\s*%").unwrap();

    re.find_iter(text)
        .take(10)
        .map(|m| m.as_str().to_string())
        .collect()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AmountMatch {
    pub value: String,
    pub unit: Option<String>,
    pub raw: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_urls() {
        let content = r"
            Check out https://example.com and
            [link](https://foo.bar/path?q=1) for more.
            Also http://old.site.org.
        ";

        let urls = extract_urls(content);
        assert_eq!(urls.len(), 3);
        assert!(urls.contains(&"https://example.com".to_string()));
        assert!(urls.contains(&"https://foo.bar/path?q=1".to_string()));
        assert!(urls.contains(&"http://old.site.org".to_string()));
    }

    #[test]
    fn test_extract_urls_dedup() {
        let content = "https://dup.com https://dup.com https://dup.com";
        let urls = extract_urls(content);
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn test_extract_amounts() {
        let text = "The market is worth $33 billion and growing to $48.2M";
        let amounts = extract_amounts(text);
        assert_eq!(amounts.len(), 2);
        assert_eq!(amounts[0].value, "33");
        assert_eq!(amounts[0].unit, Some("billion".to_string()));
        assert_eq!(amounts[1].value, "48.2");
        assert_eq!(amounts[1].unit, Some("M".to_string()));
    }

    #[test]
    fn test_extract_percentages() {
        let text = "Growth of 71% with 53% adoption rate";
        let pcts = extract_percentages(text);
        assert_eq!(pcts.len(), 2);
        assert_eq!(pcts[0], "71%");
        assert_eq!(pcts[1], "53%");
    }
}
