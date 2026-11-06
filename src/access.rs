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

/// Class or id names used by paywall overlays (matched as whole name parts)
const PAYWALL_MARKERS: [&str; 6] = [
    "paywall",
    "subscription-wall",
    "piano-offer",
    "premium-wall",
    "tp-modal",
    "regwall",
];

/// Class or id names used by login walls (matched as whole name parts)
const LOGIN_MARKERS: [&str; 5] = [
    "login-wall",
    "auth-wall",
    "signup-wall",
    "login-modal",
    "gate-content",
];

/// Name parts that turn a marker into its opposite (`no-paywall`, `paywall-free`)
const NEGATIONS: [&str; 8] = [
    "no", "not", "non", "free", "without", "hidden", "disabled", "off",
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

    if not_free_structured_data(html) || has_marker(&doc, &PAYWALL_MARKERS) {
        return Access::Paywall;
    }
    if short && contains_any(&text, &PAYWALL_PHRASES) {
        return Access::Paywall;
    }
    if has_marker(&doc, &LOGIN_MARKERS) {
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

/// True when an element's class or id carries a marker as a whole part of a name.
///
/// `paywall`, `paywall-overlay` and `article-paywall` match; `no-paywall`,
/// `paywall-free` and `paywalled-content-helper` do not.
fn has_marker(doc: &Html, markers: &[&str]) -> bool {
    doc.tree.nodes().any(|node| {
        let marked = node.value().as_element().is_some_and(|el| {
            el.classes()
                .chain(el.id())
                .any(|name| name_has_marker(&name.to_ascii_lowercase(), markers))
        });
        // A wall that is in the page but hidden (not shown to readers) does not count
        marked
            && !std::iter::once(node)
                .chain(node.ancestors())
                .any(|n| n.value().as_element().is_some_and(is_hidden))
    })
}

/// Hidden through the `hidden` attribute, `aria-hidden="true"` or an inline
/// `display: none` / `visibility: hidden` style
fn is_hidden(el: &scraper::node::Element) -> bool {
    let style: String = el
        .attr("style")
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    el.attr("hidden").is_some()
        || el.attr("aria-hidden") == Some("true")
        || style.contains("display:none")
        || style.contains("visibility:hidden")
}

fn name_has_marker(name: &str, markers: &[&str]) -> bool {
    let parts: Vec<&str> = name.split(['-', '_']).collect();
    if parts.iter().any(|p| NEGATIONS.contains(p)) {
        return false;
    }
    markers.iter().any(|marker| {
        let wanted: Vec<&str> = marker.split('-').collect();
        parts.windows(wanted.len()).any(|w| w == wanted.as_slice())
    })
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
    fn marker_names_match_whole_parts_only() {
        let open = long_article("");
        for class in [
            "no-paywall",
            "paywall-free",
            "paywalled-helper",
            "has_no_paywall",
        ] {
            let html = open.replace("<article>", &format!("<article class='{class}'>"));
            assert_eq!(detect(&html), Access::Open, "{class}");
        }
        for class in ["paywall", "article-paywall", "PAYWALL_overlay", "tp-modal"] {
            let html = open.replace("<article>", &format!("<article class='{class}'>"));
            assert_eq!(detect(&html), Access::Paywall, "{class}");
        }
        let html = open.replace("<article>", "<article id='login-modal'>");
        assert_eq!(detect(&html), Access::Login);
    }

    #[test]
    fn hidden_wall_elements_do_not_count() {
        for wrapper in [
            "<div class='tp-modal' style='display: none'>Subscribe</div>",
            "<div class='paywall' hidden>Subscribe</div>",
            "<div aria-hidden='true'><div class='paywall'>Subscribe</div></div>",
            "<section style='VISIBILITY:hidden'><div class='login-wall'>Sign in</div></section>",
        ] {
            assert_eq!(detect(&long_article(wrapper)), Access::Open, "{wrapper}");
        }
        let shown = long_article("<div class='tp-modal' style='display:block'>Subscribe</div>");
        assert_eq!(detect(&shown), Access::Paywall);
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
