//! Text and metadata from a PDF, keeping the column gaps tables need.

/// Extract a PDF's text, keeping wide gaps between text runs as runs of spaces.
///
/// pdf-extract's own plain text output writes one space for any gap, so table
/// cells drawn at separate positions on the page ("Region  Q3  Q4") come out as
/// "Region Q3 Q4" and no column can be found. [`LayoutText`] writes the same
/// text but pads a wide gap to the column where the next run starts.
///
/// Also returns the PDF's metadata, see [`PdfInfo`].
pub(super) fn extract_layout_text(
    bytes: &[u8],
) -> Result<(String, PdfInfo), pdf_extract::OutputError> {
    let mut doc = pdf_extract::Document::load_mem(bytes)?;
    let mut out = LayoutText::default();
    if doc.is_encrypted() {
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
pub(super) struct PdfInfo {
    pub(super) title: Option<String>,
    pub(super) author: Option<String>,
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
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u16::from_be_bytes(*p))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
