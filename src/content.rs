//! Turn the main content of a page into sections, in document order.
//!
//! Every readable block is kept: paragraphs, list items, tables (as Markdown),
//! block quotes, definition lists and loose text inside `div`s.
//! Code blocks are left to the separate `code` output.
//! When a section is longer than the limit it is cut and marked `truncated`,
//! never dropped silently.

use crate::text::cut_at_word;
use ego_tree::NodeRef;
use scraper::{ElementRef, Html, Node};
use serde::Serialize;

/// Maximum heading length in characters (bytes, on a character boundary)
const MAX_HEADING: usize = 200;

/// Default maximum number of sections returned for one page
pub const DEFAULT_MAX_SECTIONS: usize = 50;

/// A content section with heading hierarchy
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct Section {
    /// Heading level (1-6)
    pub level: u8,
    /// Heading text
    pub heading: String,
    /// Content under this heading (cleaned text, Markdown for lists and tables)
    pub content: String,
    /// True when the content was cut to the size limit
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Sections extracted from a page
#[derive(Debug, Default)]
pub struct Extracted {
    pub sections: Vec<Section>,
    /// True when any section was cut or sections were dropped past the section limit
    pub truncated: bool,
}

/// Elements that never hold readable content
const SKIP_ALWAYS: [&str; 14] = [
    "head", "script", "style", "noscript", "template", "svg", "canvas", "iframe", "button",
    "select", "option", "textarea", "input", "pre",
];

/// Page chrome, skipped unless `raw` is set or it sits inside the article itself.
/// Forms are not chrome: many sites (ASP.NET, for one) wrap the whole page in one;
/// their controls are skipped through [`SKIP_ALWAYS`] instead.
const CHROME: [&str; 4] = ["nav", "header", "footer", "aside"];

/// Block elements whose whole text becomes one block
const TEXT_BLOCKS: [&str; 6] = ["p", "dt", "dd", "figcaption", "address", "summary"];

/// Inline elements: their text joins the surrounding text
const INLINE: [&str; 20] = [
    "a", "abbr", "b", "bdi", "cite", "code", "data", "del", "dfn", "em", "i", "ins", "kbd", "mark",
    "q", "s", "small", "span", "strong", "time",
];

/// Extract sections from a content document.
///
/// `max_chars` limits each section's content in characters and `max_sections`
/// the number of sections; `0` means no limit for either.
/// `raw` keeps navigation, headers, footers and asides.
#[must_use]
pub fn extract_sections(doc: &Html, max_chars: usize, max_sections: usize, raw: bool) -> Extracted {
    let mut b = Builder {
        max_chars,
        max_sections,
        raw,
        ..Builder::default()
    };
    let mut buf = String::new();
    b.visit(doc.tree.root(), &mut buf, false);
    b.flush(&mut buf);
    b.finish()
}

#[derive(Default)]
struct Builder {
    sections: Vec<Section>,
    current: Option<Section>,
    max_chars: usize,
    max_sections: usize,
    raw: bool,
    truncated: bool,
}

impl Builder {
    fn visit(&mut self, node: NodeRef<'_, Node>, buf: &mut String, in_article: bool) {
        match node.value() {
            Node::Text(text) => buf.push_str(text),
            Node::Element(_) => {
                if let Some(el) = ElementRef::wrap(node) {
                    self.visit_element(el, buf, in_article);
                }
            }
            _ => {
                for child in node.children() {
                    self.visit(child, buf, in_article);
                }
            }
        }
    }

    fn visit_element(&mut self, el: ElementRef<'_>, buf: &mut String, in_article: bool) {
        let name = el.value().name();
        if SKIP_ALWAYS.contains(&name) || (!self.raw && !in_article && CHROME.contains(&name)) {
            return;
        }
        if name == "br" {
            buf.push(' ');
            return;
        }
        if INLINE.contains(&name) {
            for child in el.children() {
                self.visit(child, buf, in_article);
            }
            return;
        }

        // Every other element is a block: close the text gathered so far
        self.flush(buf);
        if let Some(level) = heading_level(name) {
            self.start_section(level, &normalized_text(el));
        } else if TEXT_BLOCKS.contains(&name) {
            self.push_block(&normalized_text(el));
        } else if name == "blockquote" {
            let text = normalized_text(el);
            if !text.is_empty() {
                self.push_block(&format!("> {text}"));
            }
        } else if name == "ul" || name == "ol" {
            let mut lines = Vec::new();
            list_lines(el, 0, &mut lines);
            self.push_block(&lines.join("\n"));
        } else if name == "table" && !has_nested_table(el) {
            if let Some(md) = table_markdown(el) {
                self.push_block(&md);
            }
        } else {
            let inside = in_article || matches!(name, "article" | "main");
            let mut inner = String::new();
            for child in el.children() {
                self.visit(child, &mut inner, inside);
            }
            self.flush(&mut inner);
        }
    }

    /// Turn gathered loose text into a block
    fn flush(&mut self, buf: &mut String) {
        let text = collapse(buf);
        buf.clear();
        self.push_block(&text);
    }

    fn start_section(&mut self, level: u8, heading: &str) {
        if heading.chars().count() < 2 {
            return;
        }
        self.close_section();
        self.current = Some(Section {
            level,
            heading: cut_at_word(heading, MAX_HEADING),
            content: String::new(),
            truncated: false,
        });
    }

    fn push_block(&mut self, text: &str) {
        if text.chars().count() < 3 {
            return;
        }
        let max = self.max_chars;
        let section = self.current.get_or_insert_with(|| Section {
            level: 1,
            heading: "Content".to_string(),
            content: String::new(),
            truncated: false,
        });
        if section.truncated {
            return;
        }
        if !section.content.is_empty() {
            section.content.push_str("\n\n");
        }
        section.content.push_str(text);
        if max > 0 {
            // Limit in characters, not bytes, so accented text is not cut short
            if let Some((byte_at_limit, _)) = section.content.char_indices().nth(max) {
                section.content = cut_at_word(&section.content, byte_at_limit);
                section.truncated = true;
                self.truncated = true;
            }
        }
    }

    fn close_section(&mut self) {
        if let Some(s) = self.current.take() {
            if !s.content.is_empty() {
                self.sections.push(s);
            }
        }
    }

    fn finish(mut self) -> Extracted {
        self.close_section();
        if self.max_sections > 0 && self.sections.len() > self.max_sections {
            self.sections.truncate(self.max_sections);
            self.truncated = true;
        }
        Extracted {
            sections: self.sections,
            truncated: self.truncated,
        }
    }
}

fn heading_level(name: &str) -> Option<u8> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

/// Collapse runs of whitespace into single spaces
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text of an element without scripts, styles and other skipped elements
fn normalized_text(el: ElementRef<'_>) -> String {
    let mut out = String::new();
    gather_text(*el, &mut out, &[]);
    collapse(&out)
}

/// Gather text below `node`, skipping [`SKIP_ALWAYS`] and the extra element names given
fn gather_text(node: NodeRef<'_, Node>, out: &mut String, also_skip: &[&str]) {
    for child in node.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                let name = e.name();
                if SKIP_ALWAYS.contains(&name) || also_skip.contains(&name) {
                    continue;
                }
                if name == "br" {
                    out.push(' ');
                } else {
                    // Keep words in neighbouring cells or blocks apart
                    if !INLINE.contains(&name) {
                        out.push(' ');
                    }
                    gather_text(child, out, also_skip);
                }
            }
            _ => {}
        }
    }
}

/// One Markdown line per list item, nested lists indented by two spaces
fn list_lines(list: ElementRef<'_>, depth: usize, lines: &mut Vec<String>) {
    let ordered = list.value().name() == "ol";
    let mut n = 0;
    for li in list.children().filter_map(ElementRef::wrap) {
        if li.value().name() != "li" {
            continue;
        }
        n += 1;
        let mut text = String::new();
        gather_text(*li, &mut text, &["ul", "ol"]);
        let text = collapse(&text);
        if !text.is_empty() {
            let marker = if ordered {
                format!("{n}.")
            } else {
                "-".to_string()
            };
            lines.push(format!("{}{marker} {text}", "  ".repeat(depth)));
        }
        for nested in li.children().filter_map(ElementRef::wrap) {
            if matches!(nested.value().name(), "ul" | "ol") {
                list_lines(nested, depth + 1, lines);
            }
        }
    }
}

fn has_nested_table(table: ElementRef<'_>) -> bool {
    table
        .descendants()
        .skip(1)
        .filter_map(ElementRef::wrap)
        .any(|e| e.value().name() == "table")
}

/// Render a table as Markdown: first row is the header.
///
/// Returns `None` for tables with no cells or a single column
/// (usually layout, whose text is kept as plain blocks instead).
fn table_markdown(table: ElementRef<'_>) -> Option<String> {
    let rows: Vec<Vec<String>> = table
        .descendants()
        .filter_map(ElementRef::wrap)
        .filter(|e| e.value().name() == "tr")
        .map(|tr| {
            tr.children()
                .filter_map(ElementRef::wrap)
                .filter(|c| matches!(c.value().name(), "td" | "th"))
                .map(|c| normalized_text(c).replace('|', "\\|"))
                .collect::<Vec<_>>()
        })
        .filter(|r| !r.is_empty())
        .collect();

    let width = rows.iter().map(Vec::len).max()?;
    if width < 2 {
        let text = rows.into_iter().flatten().collect::<Vec<_>>().join("\n\n");
        return (!text.trim().is_empty()).then_some(text);
    }

    let line = |r: &[String]| {
        let mut cells: Vec<&str> = r.iter().map(String::as_str).collect();
        cells.resize(width, "");
        format!("| {} |", cells.join(" | "))
    };
    let mut out = vec![line(&rows[0]), format!("|{}", " --- |".repeat(width))];
    out.extend(rows[1..].iter().map(|r| line(r)));
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections(html: &str) -> Extracted {
        extract_sections(&Html::parse_document(html), 0, 0, false)
    }

    #[test]
    fn keeps_blocks_in_document_order() {
        let html =
            "<h1>Alpha</h1><p>First paragraph here.</p><h2>Beta</h2><p>Second paragraph.</p>";
        let out = sections(html);
        assert_eq!(out.sections.len(), 2);
        assert_eq!(out.sections[0].heading, "Alpha");
        assert_eq!(out.sections[0].level, 1);
        assert_eq!(out.sections[1].heading, "Beta");
        assert_eq!(out.sections[1].level, 2);
        assert_eq!(out.sections[1].content, "Second paragraph.");
        assert!(!out.truncated);
    }

    #[test]
    fn every_heading_level_is_recognised() {
        let html = (1..=6)
            .map(|n| format!("<h{n}>Heading {n}</h{n}><p>Body {n} text.</p>"))
            .collect::<Vec<_>>()
            .concat();
        let out = sections(&html);
        let levels: Vec<u8> = out.sections.iter().map(|s| s.level).collect();
        assert_eq!(levels, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn text_before_first_heading_goes_into_content_section() {
        let out =
            sections("<p>Intro paragraph before any heading.</p><h2>Next</h2><p>More text.</p>");
        assert_eq!(out.sections[0].heading, "Content");
        assert_eq!(
            out.sections[0].content,
            "Intro paragraph before any heading."
        );
    }

    #[test]
    fn keeps_lists_tables_quotes_and_loose_text() {
        let html = r"<article><h1>Report</h1>
            <ul><li>First item</li><li>Second <b>bold</b> item<ul><li>Nested item</li></ul></li></ul>
            <ol><li>Step one</li><li>Step two</li></ol>
            <table><tr><th>Plan</th><th>Price</th></tr><tr><td>Free</td><td>$0</td></tr><tr><td>Pro | Team</td></tr></table>
            <blockquote><p>Quoted words here.</p></blockquote>
            <div>Loose text in a div.<br>Same block.</div>
            <dl><dt>Term</dt><dd>Definition text</dd></dl>
            <figure><figcaption>Figure caption</figcaption></figure>
            <blockquote> </blockquote>
            </article>";
        let content = &sections(html).sections[0].content;
        for expected in [
            "- First item",
            "- Second bold item",
            "  - Nested item",
            "1. Step one\n2. Step two",
            "| Plan | Price |\n| --- | --- |\n| Free | $0 |\n| Pro \\| Team |  |",
            "> Quoted words here.",
            "Loose text in a div. Same block.",
            "Term",
            "Definition text",
            "Figure caption",
        ] {
            assert!(content.contains(expected), "{expected:?}");
        }
    }

    #[test]
    fn skips_chrome_scripts_and_code_outside_article() {
        let html = r"<body><nav>Home Blog About</nav><header>Site header text</header>
            <script>var hidden = 1;</script><style>.x{}</style>
            <main><header><h1>Kept title</h1></header><p>Kept body text.</p>
            <pre><code>fn main() {}</code></pre><footer>Article footer note</footer></main>
            <footer>Site footer text</footer><form><input type='password'><button>Log in</button></form></body>";
        let out = sections(html);
        let all: String = out
            .sections
            .iter()
            .map(|s| format!("{} {}", s.heading, s.content))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("Kept title"));
        assert!(all.contains("Kept body text."));
        assert!(all.contains("Article footer note"));
        for gone in [
            "Home Blog",
            "Site header",
            "hidden",
            "fn main",
            "Site footer",
            "Log in",
        ] {
            assert!(!all.contains(gone), "{gone:?} should be skipped: {all}");
        }
    }

    #[test]
    fn raw_mode_keeps_chrome() {
        let doc = Html::parse_document("<nav>Navigation links</nav><p>Body text.</p>");
        let out = extract_sections(&doc, 0, 0, true);
        assert!(out.sections[0].content.contains("Navigation links"));
    }

    #[test]
    fn long_sections_are_cut_and_flagged() {
        let body = "word ".repeat(2000);
        let html = format!("<h1>Long</h1><p>{body}</p><p>Never reached.</p>");
        let out = extract_sections(&Html::parse_document(&html), 500, 0, false);
        let s = &out.sections[0];
        assert!(s.truncated);
        assert!(out.truncated);
        assert!(s.content.len() <= 503);
        assert!(!s.content.contains("Never reached"));
    }

    #[test]
    fn limit_counts_characters_not_bytes() {
        // 300 two-byte characters fit a 400-character limit
        let body = "é".repeat(300);
        let out = extract_sections(
            &Html::parse_document(&format!("<p>{body}</p>")),
            400,
            0,
            false,
        );
        assert!(!out.truncated);
        let out = extract_sections(
            &Html::parse_document(&format!("<p>{body}</p>")),
            100,
            0,
            false,
        );
        assert!(out.truncated);
        assert!(out.sections[0].content.chars().count() <= 103);
    }

    #[test]
    fn page_wrapped_in_a_form_keeps_its_content() {
        let out = sections("<form id='aspnetForm'><h1>Title</h1><p>Body text here.</p><input name='x'><button>Go</button></form>");
        assert_eq!(out.sections[0].heading, "Title");
        assert_eq!(out.sections[0].content, "Body text here.");
    }

    #[test]
    fn zero_limit_keeps_everything() {
        let body = "word ".repeat(2000);
        let out = sections(&format!("<h1>Long</h1><p>{body}</p>"));
        assert!(!out.truncated);
        assert_eq!(out.sections[0].content.len(), body.trim_end().len());
    }

    #[test]
    fn too_many_sections_are_flagged() {
        let html: String = (0..60)
            .map(|n| format!("<h2>Part {n}</h2><p>Text {n}.</p>"))
            .collect::<Vec<_>>()
            .concat();
        let doc = Html::parse_document(&html);
        let out = extract_sections(&doc, 0, DEFAULT_MAX_SECTIONS, false);
        assert_eq!(out.sections.len(), DEFAULT_MAX_SECTIONS);
        assert!(out.truncated);
        let all = extract_sections(&doc, 0, 0, false);
        assert_eq!(all.sections.len(), 60);
        assert!(!all.truncated);
    }

    #[test]
    fn empty_and_tiny_blocks_are_ignored() {
        let out = sections("<h1>A</h1><h2>Real</h2><p>ok</p><p>Kept text.</p><h3>Empty</h3>");
        assert_eq!(out.sections.len(), 1);
        assert_eq!(out.sections[0].heading, "Real");
        assert_eq!(out.sections[0].content, "Kept text.");
    }

    #[test]
    fn layout_and_nested_tables_keep_their_text() {
        let html = "<table><tr><td><p>Layout cell paragraph.</p></td></tr></table>\
                    <table><tr><td><table><tr><td>Inner a</td><td>Inner b</td></tr></table></td></tr></table>\
                    <table></table>";
        let content = &sections(html).sections[0].content;
        assert!(content.contains("Layout cell paragraph."));
        assert!(content.contains("| Inner a | Inner b |"));
    }

    #[test]
    fn inline_elements_in_containers_join_the_text() {
        let out = sections("<div>Read <a href='/x'>the <b>full</b> report</a> today.</div>");
        assert_eq!(out.sections[0].content, "Read the full report today.");
    }

    #[test]
    fn comments_inside_blocks_are_ignored() {
        let out = sections("<p>Before <!-- hidden note --> after.</p>");
        assert_eq!(out.sections[0].content, "Before after.");
    }

    #[test]
    fn comments_and_doctype_are_ignored() {
        let out = sections("<!DOCTYPE html><!-- note --><p>Visible text.</p>");
        assert_eq!(out.sections[0].content, "Visible text.");
    }

    #[test]
    fn list_items_skip_non_li_children_and_empty_items() {
        let html = "<ul><li></li><span>stray</span><li><span>Real item</span></li></ul>";
        assert_eq!(sections(html).sections[0].content, "- Real item");
    }

    #[test]
    fn gathered_text_breaks_between_blocks_and_skips_scripts() {
        let html =
            "<p>One<br>two</p><blockquote><p>Alpha</p><p>Beta</p><script>x</script></blockquote>";
        let content = &sections(html).sections[0].content;
        assert!(content.contains("One two"));
        assert!(content.contains("> Alpha Beta"));
    }
}
