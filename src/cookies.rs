//! Netscape cookie files (the `cookies.txt` format exported by browsers and curl).

/// One cookie from a cookie file
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// Expiry as seconds since the Unix epoch; `None` for session cookies
    pub expires: Option<i64>,
    pub name: String,
    pub value: String,
}

/// Parse a Netscape cookie file.
///
/// Each line holds seven tab-separated fields:
/// domain, include-subdomains flag, path, secure flag, expiry, name, value.
/// Lines starting with `#` are comments, except the `#HttpOnly_` prefix.
///
/// # Errors
///
/// Returns the 1-based line number and a reason for the first malformed line.
pub fn parse(text: &str) -> Result<Vec<Cookie>, String> {
    let mut cookies = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        let (line, http_only) = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => (rest, true),
            None => (line, false),
        };
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [domain, _subdomains, path, secure, expires, name, value] = fields[..] else {
            return Err(format!(
                "line {}: expected 7 tab-separated fields, found {}",
                i + 1,
                fields.len()
            ));
        };
        let expires: i64 = expires
            .trim()
            .parse()
            .map_err(|_| format!("line {}: expiry {expires:?} is not a number", i + 1))?;
        cookies.push(Cookie {
            domain: domain.to_string(),
            path: path.to_string(),
            secure: secure.eq_ignore_ascii_case("TRUE"),
            http_only,
            expires: (expires > 0).then_some(expires),
            name: name.to_string(),
            value: value.to_string(),
        });
    }
    Ok(cookies)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookies_comments_and_http_only() {
        let text = "# Netscape HTTP Cookie File\n\
                    \n\
                    .example.com\tTRUE\t/\tTRUE\t1893456000\tsession\tabc123\r\n\
                    #HttpOnly_example.org\tFALSE\t/app\tFALSE\t0\ttoken\tx=y\n";
        let cookies = parse(text).unwrap();
        assert_eq!(cookies.len(), 2);
        assert_eq!(
            cookies[0],
            Cookie {
                domain: ".example.com".into(),
                path: "/".into(),
                secure: true,
                http_only: false,
                expires: Some(1_893_456_000),
                name: "session".into(),
                value: "abc123".into(),
            }
        );
        assert!(cookies[1].http_only);
        assert!(!cookies[1].secure);
        assert_eq!(cookies[1].expires, None);
        assert_eq!(cookies[1].value, "x=y");
    }

    #[test]
    fn reports_malformed_lines() {
        let err = parse("# header\nexample.com\tTRUE\t/\n").unwrap_err();
        assert!(err.starts_with("line 2:"), "{err}");

        let err = parse("example.com\tTRUE\t/\tFALSE\tsoon\tname\tvalue").unwrap_err();
        assert!(err.contains("not a number"), "{err}");
    }

    #[test]
    fn empty_file_has_no_cookies() {
        assert_eq!(parse("").unwrap(), Vec::new());
    }
}
