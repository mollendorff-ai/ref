//! URL extraction from markdown and text content

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

/// http(s) URLs in Markdown or plain text
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s\)>\]"'`]+"#).expect("constant regex"));

/// Money amounts, sign before or after the number (see [`extract_amounts`])
static AMOUNT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        (?P<pc>CA\$|US\$|C\$|A\$|[$€£]) \s? (?P<pv>\d(?:[\d,.]*\d)?) (?:\s*(?P<pu>billion|million|B|M|K)\b)?
        |
        \b(?P<sv>\d(?:[\d,.]*\d)?) (?:\s*(?P<su>billion|million|B|M|K))? \s? (?P<sc>CA\$|US\$|C\$|A\$|[$€£])
        ",
    )
    .expect("constant regex")
});

/// Percentages: a number, then `%`
static PERCENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d(?:[\d,.]*\d)?\s*%").expect("constant regex"));

/// Extract unique URLs from text content.
#[must_use]
pub fn extract_urls(content: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut urls = Vec::new();

    for mat in URL_RE.find_iter(content) {
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

/// Extract money amounts from text.
///
/// Two spellings are recognised, with `$`, `€`, `£`, `CA$`, `US$`, `C$` or `A$`:
/// the sign before the number (`$33 billion`, `€1.2M`) and the sign after it
/// (`4,5 M$`, `150 €`, common in French).
/// A number is needed on the sign's number side, so `M$. Les` gives nothing.
#[must_use]
pub fn extract_amounts(text: &str) -> Vec<AmountMatch> {
    AMOUNT_RE
        .captures_iter(text)
        .take(10)
        .map(|cap| AmountMatch {
            value: (cap.name("pv").or_else(|| cap.name("sv")))
                .map_or_else(String::new, |m| m.as_str().to_string()),
            unit: cap
                .name("pu")
                .or_else(|| cap.name("su"))
                .map(|m| m.as_str().to_string()),
            currency: cap
                .name("pc")
                .or_else(|| cap.name("sc"))
                .map_or_else(String::new, |m| m.as_str().to_string()),
            raw: cap[0].to_string(),
        })
        .collect()
}

/// Extract percentages from text.
///
/// A digit is needed, so a lone `.` or `,` before a `%` is not a percentage.
#[must_use]
pub fn extract_percentages(text: &str) -> Vec<String> {
    PERCENT_RE
        .find_iter(text)
        .take(10)
        .map(|m| m.as_str().to_string())
        .collect()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AmountMatch {
    pub value: String,
    pub unit: Option<String>,
    /// Currency sign as written: `$`, `€`, `£`, `CA$`, `US$`, `C$` or `A$`
    pub currency: String,
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
        assert_eq!(amounts[0].currency, "$");
        assert_eq!(amounts[0].raw, "$33 billion");
    }

    #[test]
    fn test_extract_amounts_sign_after_the_number() {
        let amounts = extract_amounts("revenus de 4,5 M$ et des frais de 150 € ou 12\u{a0}K$.");
        assert_eq!(amounts.len(), 3);
        assert_eq!(amounts[0].value, "4,5");
        assert_eq!(amounts[0].unit, Some("M".to_string()));
        assert_eq!(amounts[0].currency, "$");
        assert_eq!(amounts[0].raw, "4,5 M$");
        assert_eq!(amounts[1].value, "150");
        assert_eq!(amounts[1].unit, None);
        assert_eq!(amounts[1].currency, "€");
        assert_eq!(amounts[2].value, "12");
        assert_eq!(amounts[2].unit, Some("K".to_string()));
    }

    #[test]
    fn test_extract_amounts_other_currencies() {
        let amounts = extract_amounts("€1.2M, £300, CA$50 and US$7 billion");
        let got: Vec<_> = amounts
            .iter()
            .map(|a| (a.currency.as_str(), a.value.as_str(), a.unit.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("€", "1.2", Some("M")),
                ("£", "300", None),
                ("CA$", "50", None),
                ("US$", "7", Some("billion")),
            ]
        );
    }

    #[test]
    fn test_extract_amounts_needs_a_digit() {
        // A sign with no number beside it is not an amount (it used to give ".")
        assert!(extract_amounts("coûts en M$. Les revenus").is_empty());
        assert!(extract_amounts("price: $ or $. and $,").is_empty());
        // Trailing punctuation is not part of the number, and no stray space in `raw`
        let amounts = extract_amounts("It costs $5. Then $6, then $7 Mondays.");
        let raw: Vec<_> = amounts.iter().map(|a| a.raw.as_str()).collect();
        assert_eq!(raw, ["$5", "$6", "$7"]);
        assert!(amounts.iter().all(|a| a.unit.is_none()));
    }

    #[test]
    fn test_extract_percentages() {
        let text = "Growth of 71% with 53% adoption rate";
        let pcts = extract_percentages(text);
        assert_eq!(pcts.len(), 2);
        assert_eq!(pcts[0], "71%");
        assert_eq!(pcts[1], "53%");
    }

    #[test]
    fn test_extract_percentages_needs_a_digit() {
        assert_eq!(
            extract_percentages("up 12 %, down 3,5% and 0.5%"),
            ["12 %", "3,5%", "0.5%"]
        );
        assert_eq!(
            extract_percentages("a . % or , % sign"),
            Vec::<String>::new()
        );
    }
}
