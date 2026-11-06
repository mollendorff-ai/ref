//! Paywall and login-wall detection, shared by `fetch` and `verify-refs`.
//!
//! A page is only flagged when there is a real signal of a wall:
//! structured data that says the article is not free, an overlay element
//! that walls use, a password form, or wall wording on a page that has
//! little readable text left.
//! Words such as "paywall" inside a normal article do not count.

use regex::Regex;
use scraper::{Html, Node, Selector};

/// Pages with less visible text than this are treated as "walled" when they
/// also carry wall wording or a password field.
const SHORT_PAGE_CHARS: usize = 1500;

/// What stands between the reader and the content
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Open,
    Paywall,
    Login,
}

const PAYWALL_PHRASES: [&str; 9] = [
    "subscribe to continue",
    "subscription required",
    "subscribe to read",
    "unlock this article",
    "purchase to read",
    "buy now to read",
    "for paid subscribers",
    "this content is for members only",
    "become a member to read",
];

const LOGIN_PHRASES: [&str; 8] = [
    "sign in to continue",
    "log in to continue",
    "login to continue",
    "please sign in",
    "please log in",
    "sign up to view",
    "register to view",
    "authentication required",
];

const PAYWALL_ELEMENTS: [&str; 6] = [
    "[class*='paywall']",
    "[id*='paywall']",
    "[class*='subscription-wall']",
    "[class*='piano-offer']",
    "[class*='premium-wall']",
    "[class*='tp-modal']",
];

const LOGIN_ELEMENTS: [&str; 5] = [
    "[class*='login-wall']",
    "[class*='auth-wall']",
    "[class*='signup-wall']",
    "[id*='login-modal']",
    "[class*='gate-content']",
];

/// Decide whether a rendered page is open, paywalled or behind a login.
///
/// # Panics
///
/// Panics if the internal structured-data regex fails to compile (it is a constant).
#[must_use]
pub fn detect(html: &str) -> Access {
    let doc = Html::parse_document(html);
    let text = visible_text(&doc).to_lowercase();
    let short = text.chars().count() < SHORT_PAGE_CHARS;

    if not_free_structured_data(html) || any_element(&doc, &PAYWALL_ELEMENTS) {
        return Access::Paywall;
    }
    if short && contains_any(&text, &PAYWALL_PHRASES) {
        return Access::Paywall;
    }
    if any_element(&doc, &LOGIN_ELEMENTS) {
        return Access::Login;
    }
    if short
        && (contains_any(&text, &LOGIN_PHRASES) || any_element(&doc, &["input[type='password']"]))
    {
        return Access::Login;
    }
    Access::Open
}

/// Schema.org `isAccessibleForFree: false`, the signal publishers give search engines.
fn not_free_structured_data(html: &str) -> bool {
    let re = Regex::new(r#"(?i)"?isAccessibleForFree"?\s*(?::|content=)\s*"?false"#)
        .expect("constant regex");
    re.is_match(html)
}

fn any_element(doc: &Html, selectors: &[&str]) -> bool {
    selectors
        .iter()
        .filter_map(|s| Selector::parse(s).ok())
        .any(|sel| doc.select(&sel).next().is_some())
}

fn contains_any(text: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|p| text.contains(p))
}

/// Text a reader would see: everything outside `head`, scripts and styles,
/// with whitespace collapsed.
fn visible_text(doc: &Html) -> String {
    let mut words: Vec<&str> = Vec::new();
    for node in doc.tree.nodes() {
        let Node::Text(text) = node.value() else {
            continue;
        };
        let hidden = node.ancestors().any(|a| {
            a.value().as_element().is_some_and(|e| {
                matches!(
                    e.name(),
                    "head" | "script" | "style" | "noscript" | "template"
                )
            })
        });
        if !hidden {
            words.extend(text.split_whitespace());
        }
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_article(extra: &str) -> String {
        let body = "Readable article text that goes on for a while. ".repeat(60);
        format!("<html><body><article><h1>Title</h1><p>{body}</p>{extra}</article></body></html>")
    }

    #[test]
    fn plain_page_is_open() {
        assert_eq!(detect("<div>Normal content here</div>"), Access::Open);
    }

    #[test]
    fn article_about_paywalls_is_open() {
        let html = "<html><head><title>Why news sites love the paywall</title></head><body>\
                    <article><h1>The paywall economy</h1><p>Publishers rely on a paywall.</p></article>\
                    </body></html>";
        assert_eq!(detect(html), Access::Open);
    }

    #[test]
    fn short_page_with_wall_wording_is_paywall() {
        assert_eq!(
            detect("<div>Subscribe to continue reading</div>"),
            Access::Paywall
        );
    }

    #[test]
    fn long_article_with_footer_wording_is_open() {
        let html = long_article("<footer>Subscribe to continue reading our newsletter</footer>");
        assert_eq!(detect(&html), Access::Open);
        let html = long_article("<p>Please sign in to continue commenting.</p>");
        assert_eq!(detect(&html), Access::Open);
    }

    #[test]
    fn overlay_element_is_paywall_even_on_long_pages() {
        let html = long_article("<div class='paywall-overlay'>Subscribe</div>");
        assert_eq!(detect(&html), Access::Paywall);
    }

    #[test]
    fn structured_data_marks_paywall() {
        let json_ld = r#"<html><head><script type="application/ld+json">
            {"@type":"NewsArticle","isAccessibleForFree": false}</script></head>
            <body><p>Teaser</p></body></html>"#;
        assert_eq!(detect(json_ld), Access::Paywall);

        let quoted =
            r#"<script type="application/ld+json">{"isAccessibleForFree":"False"}</script>"#;
        assert_eq!(detect(quoted), Access::Paywall);

        let meta = r#"<meta itemprop="isAccessibleForFree" content="false">"#;
        assert_eq!(detect(meta), Access::Paywall);

        let free = r#"<script type="application/ld+json">{"isAccessibleForFree": true}</script>"#;
        assert_eq!(detect(free), Access::Open);
    }

    #[test]
    fn login_wording_on_short_page_is_login() {
        assert_eq!(
            detect("<div>Please sign in to continue</div>"),
            Access::Login
        );
    }

    #[test]
    fn password_form_on_short_page_is_login() {
        let html = "<html><body><h1>Welcome</h1><form><input type='password'></form></body></html>";
        assert_eq!(detect(html), Access::Login);
    }

    #[test]
    fn password_form_on_long_page_is_open() {
        let html = long_article("<form><input type='password'></form>");
        assert_eq!(detect(&html), Access::Open);
    }

    #[test]
    fn login_wall_element_is_login() {
        let html = long_article("<div class='login-wall'>Members area</div>");
        assert_eq!(detect(&html), Access::Login);
    }

    #[test]
    fn scripts_and_styles_do_not_count_as_text() {
        let script = "var x = 1; ".repeat(500);
        let html = format!(
            "<html><head><style>body{{}}</style></head><body><script>{script}</script>\
             <p>Please log in</p></body></html>"
        );
        assert_eq!(detect(&html), Access::Login);
    }
}
