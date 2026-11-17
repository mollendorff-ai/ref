//! A small web server for tests that need real pages, and helpers that run
//! `ref` in-process against it. Each page here stands for a real bug found in
//! review: 404 pages reported as ok, titles containing "500" reported as
//! errors, tables and lists dropped, long pages cut silently, and articles
//! mentioning paywalls flagged as walled.

use serde_json::Value;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::thread;

/// Serve a few fixed pages on 127.0.0.1 and return the base URL.
pub(crate) fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || handle(stream));
        }
    });
    format!("http://{addr}")
}

fn handle(mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    // A request that cannot be read is empty, and gets the 404 page
    let mut request_line = String::new();
    let _ = reader.read_line(&mut request_line);
    // Read headers (kept for the cookie check)
    let mut headers = String::new();
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
        headers.push_str(&line.to_ascii_lowercase());
        line.clear();
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let is_pdf = std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    if is_pdf && path != "/signed-in.pdf" {
        serve_pdf(path, &mut stream);
        return;
    }
    if path == "/cross-host" {
        // Like a site that moves to another host name: same server, other name
        let port = headers
            .lines()
            .find_map(|l| l.strip_prefix("host:"))
            .and_then(|h| h.trim().rsplit(':').next())
            .unwrap_or("80")
            .to_string();
        let head = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/report\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(head.as_bytes());
        return;
    }
    if path == "/whoami" {
        let cookie = headers
            .lines()
            .find_map(|l| l.strip_prefix("cookie:"))
            .unwrap_or(" none")
            .trim()
            .to_string();
        let body = format!(
            "<html><head><title>Who</title></head><body><p>Cookie header: {cookie}</p></body></html>"
        );
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        return;
    }
    if path == "/needs-declared-agent" {
        // Like SEC EDGAR: refuses a user agent that does not carry an email
        let declared = headers
            .lines()
            .find_map(|l| l.strip_prefix("user-agent:"))
            .is_some_and(|ua| ua.contains("jane@example.com"));
        let (status, body) = if declared {
            (
                "200 OK",
                "<html><head><title>Filing</title></head><body><p>The declared reader gets the filing.</p></body></html>",
            )
        } else {
            (
                "403 Forbidden",
                "<html><head><title>Undeclared</title></head><body><p>Undeclared automated tool.</p></body></html>",
            )
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        return;
    }
    if path == "/signed-in.pdf" {
        // Only for readers who send the session cookie, like a members-only PDF
        let pdf: &[u8] = include_bytes!("../tests/fixtures/signed-in.pdf");
        let (status, body): (&str, &[u8]) = if headers.contains("sid=letmein") {
            ("200 OK", pdf)
        } else {
            ("403 Forbidden", b"forbidden")
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
        return;
    }
    let (status, extra, body) = route(path);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        if std::path::Path::new(path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json")) { "application/json" } else { "text/html; charset=utf-8" },
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

/// PDF downloads that go wrong in the ways real servers do
fn serve_pdf(path: &str, stream: &mut TcpStream) {
    let pdf: &[u8] = include_bytes!("../tests/fixtures/report.pdf");
    let head = |extra: &str, length: usize| {
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\n{extra}Content-Length: {length}\r\nConnection: close\r\n\r\n")
    };
    match path {
        // Answers, but only after a pause: longer than a short download limit
        "/slow.pdf" => {
            thread::sleep(std::time::Duration::from_secs(2));
            let _ = stream.write_all(head("", pdf.len()).as_bytes());
            let _ = stream.write_all(pdf);
        }
        // Promises 1,500 bytes (under the size limit), sends 10, then stalls
        "/stall.pdf" => {
            let _ = stream.write_all(head("", 1500).as_bytes());
            let _ = stream.write_all(&pdf[..10]);
            let _ = stream.flush();
            thread::sleep(std::time::Duration::from_secs(3));
        }
        // Says up front that it is huge
        "/big.pdf" => {
            let _ = stream.write_all(head("", 99_999_999).as_bytes());
        }
        // No length up front: only the bytes themselves show it is huge
        "/chunked.pdf" => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            );
            for _ in 0..3 {
                let _ = stream.write_all(b"3e8\r\n");
                let _ = stream.write_all(&[b'x'; 1000]);
                let _ = stream.write_all(b"\r\n");
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        // Not a PDF at all
        "/text.pdf" => {
            let body = b"plain text, not a pdf";
            let _ = stream.write_all(head("", body.len()).as_bytes());
            let _ = stream.write_all(body);
        }
        // Cuts the connection in the middle of the body
        "/cut.pdf" => {
            let _ = stream.write_all(head("", pdf.len() + 50).as_bytes());
            let _ = stream.write_all(pdf);
        }
        _ => {
            let _ = stream.write_all(head("", pdf.len()).as_bytes());
            let _ = stream.write_all(pdf);
        }
    }
}

#[allow(clippy::too_many_lines)] // one flat table of test pages
fn route(path: &str) -> (&'static str, String, String) {
    let page = |title: &str, body: &str| {
        format!(
            "<!doctype html><html><head><title>{title}</title></head><body>{body}</body></html>"
        )
    };
    match path {
        "/report" => (
            "200 OK",
            String::new(),
            page(
                "Quarterly report",
                "<nav>Home | Blog</nav><main><h1>Quarterly report</h1><p>Intro paragraph.</p>\
                 <ul><li>Revenue up 12%</li><li>Costs flat</li></ul>\
                 <table><tr><th>Region</th><th>Q3</th></tr><tr><td>North</td><td>150</td></tr></table>\
                 <blockquote>Strong quarter, said the CFO.</blockquote>\
                 <div>Loose closing remark.</div></main><footer>Site footer</footer>",
            ),
        ),
        "/fortune" => (
            "200 OK",
            String::new(),
            page("Fortune 500 Companies 2026", "<p>The largest companies ranked by revenue.</p>"),
        ),
        "/paywall-article" => (
            "200 OK",
            String::new(),
            page(
                "Why news sites love the paywall",
                "<article><h1>The paywall economy</h1><p>Publishers rely on a paywall to fund journalism.</p></article>",
            ),
        ),
        "/login" => (
            "200 OK",
            String::new(),
            page(
                "Sign in",
                "<h1>Sign in to continue</h1><form><input type='password'></form>",
            ),
        ),
        "/long" => {
            let text = "Sentence with a unique fact worth keeping. ".repeat(400);
            ("200 OK", String::new(), page("Long", &format!("<article><h1>Long</h1><p>{text}</p></article>")))
        }
        "/late" => (
            "200 OK",
            String::new(),
            page(
                "Loading",
                "<div id='app'></div><script>fetch('/data.json').then(r=>r.json()).then(d=>{\
                 document.getElementById('app').innerHTML='<h1>'+d.t+'</h1><p>'+d.p+'</p>';});</script>",
            ),
        ),
        "/data.json" => (
            "200 OK",
            String::new(),
            r#"{"t":"Loaded later","p":"This paragraph arrived through fetch() after the page loaded."}"#.to_string(),
        ),
        "/aspnet" => (
            "200 OK",
            String::new(),
            page(
                "Form page",
                "<form id='aspnetForm' method='post'><h1>Annual report</h1><p>Revenue rose this year.</p><input type='hidden' name='v'></form>",
            ),
        ),
        "/walled" => (
            "200 OK",
            String::new(),
            "<!doctype html><html><head><title>Members</title>\
             <script type='application/ld+json'>{\"isAccessibleForFree\": false}</script>\
             </head><body><p>Subscribe to read.</p></body></html>"
                .to_string(),
        ),
        "/empty" => ("200 OK", String::new(), page("Empty", "<div id='app'></div>")),
        "/no-paywall" => (
            "200 OK",
            String::new(),
            page("Open", "<article class='no-paywall'><h1>Open article</h1><p>Free to read for everyone.</p></article>"),
        ),
        "/components" => (
            "200 OK",
            String::new(),
            page(
                "Components",
                "<main><h1>Components</h1><x-card></x-card></main><script>\
                 customElements.define('x-card', class extends HTMLElement { constructor() { super();\
                 this.attachShadow({mode: 'open'}).innerHTML = '<p>Text that lives in a shadow root.</p>'; } });\
                 </script>",
            ),
        ),
        "/hidden-overlay" => {
            let text = "Open article text for everyone to read. ".repeat(60);
            (
                "200 OK",
                String::new(),
                page(
                    "Open",
                    &format!("<article><h1>Open</h1><p>{text}</p></article><div class='tp-modal' style='display:none'>Subscribe</div>"),
                ),
            )
        }
        "/sign-in" => (
            "200 OK",
            "Set-Cookie: session=kept-between-runs; Max-Age=3600; Path=/\r\n".to_string(),
            page("Signed in", "<p>You are now signed in to this test site.</p>"),
        ),
        // Signs in for /signed-in.pdf, like a members' area that hands out PDFs
        "/pdf-sign-in" => (
            "200 OK",
            "Set-Cookie: sid=letmein; Max-Age=3600; Path=/\r\n".to_string(),
            page("Signed in", "<p>You can now read the members' PDF.</p>"),
        ),
        "/moved" => ("301 Moved Permanently", "Location: /report\r\n".to_string(), String::new()),
        "/broken" => ("500 Internal Server Error", String::new(), page("Oops", "<p>Server error page.</p>")),
        _ => ("404 Not Found", String::new(), page("Not found", "<p>Nothing here.</p>")),
    }
}

/// A one-page PDF with `lines` of text, one under the other (none: a page
/// without text)
pub(crate) fn pdf_with_lines(lines: &[&str]) -> Vec<u8> {
    let mut y = 780.0;
    let placed: Vec<(f32, f32, &str)> = lines
        .iter()
        .map(|line| {
            y -= 12.0;
            (20.0, y, *line)
        })
        .collect();
    pdf_with_text(&placed)
}

/// A one-page PDF with each text drawn at its own (x, y)
pub(crate) fn pdf_with_text(items: &[(f32, f32, &str)]) -> Vec<u8> {
    let mut content = String::from("BT /F1 10 Tf\n");
    for (x, y, text) in items {
        let text = text
            .replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)");
        writeln!(content, "1 0 0 1 {x} {y} Tm ({text}) Tj").unwrap();
    }
    content.push_str("ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".to_string(),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        write!(pdf, "{} 0 obj\n{object}\nendobj\n", i + 1).unwrap();
    }
    let xref = pdf.len();
    let count = objects.len() + 1;
    write!(pdf, "xref\n0 {count}\n0000000000 65535 f \n").unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        pdf,
        "trailer\n<< /Size {count} /Root 1 0 R /ID [<00112233445566778899aabbccddeeff> <00112233445566778899aabbccddeeff>] >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .unwrap();
    pdf.into_bytes()
}

/// Run `ref <args>` in-process with no standard input:
/// (exit code, standard output, error output)
pub(crate) async fn ref_cmd(args: &[&str]) -> (ExitCode, String, String) {
    ref_cmd_with_input(args, "").await
}

/// Run `ref <args>` in-process with `input` as its standard input
pub(crate) async fn ref_cmd_with_input(args: &[&str], input: &str) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let line = std::iter::once("ref")
        .chain(args.iter().copied())
        .map(Into::into)
        .collect();
    let stop = Box::pin(std::future::pending());
    let code = crate::cli::execute(line, &mut input.as_bytes(), &mut out, &mut err, stop).await;
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    (code, text(out), text(err))
}

/// Run `ref fetch --profile fresh <args>`: (exit code, its JSON)
pub(crate) async fn fetch(args: &[&str]) -> (ExitCode, Value) {
    fetch_with_profile("fresh", args).await
}

/// Run `ref fetch --profile <profile> <args>`: (exit code, its JSON); panics
/// with ref's error output when it printed no JSON
pub(crate) async fn fetch_with_profile(profile: &str, args: &[&str]) -> (ExitCode, Value) {
    let mut line = vec!["fetch", "--profile", profile];
    line.extend_from_slice(args);
    let (code, out, err) = ref_cmd(&line).await;
    let json = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("invalid JSON ({e}): {out}\nstderr: {err}"));
    (code, json)
}

pub(crate) fn all_content(page: &Value) -> String {
    page["sections"]
        .as_array()
        .map(|s| {
            s.iter()
                .map(|x| {
                    format!(
                        "{} {}",
                        x["heading"].as_str().unwrap_or(""),
                        x["content"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[should_panic(expected = "invalid JSON")]
    async fn fetch_says_when_ref_printed_no_json() {
        // Refused before Chrome starts, so ref prints an error and no JSON
        super::fetch(&["--user-agent", " ", "https://example.com"]).await;
    }
}
