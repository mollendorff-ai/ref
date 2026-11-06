//! End-to-end tests against a local HTTP server (needs Chrome, see ADR-003).
//!
//! Each case here was a real bug found in review: 404 pages reported as ok,
//! titles containing "500" reported as errors, tables and lists dropped,
//! long pages cut silently, and articles mentioning paywalls flagged as walled.
//!
//! Run with `cargo test -- --ignored`.

use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

/// Serve a few fixed pages on 127.0.0.1 and return the base URL.
fn serve() -> String {
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
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // Read headers (kept for the cookie check)
    let mut headers = String::new();
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
        headers.push_str(&line.to_ascii_lowercase());
        line.clear();
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    if path == "/signed-in.pdf" {
        // Only for readers who send the session cookie, like a members-only PDF
        let pdf: &[u8] = include_bytes!("fixtures/signed-in.pdf");
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
        "/moved" => ("301 Moved Permanently", "Location: /report\r\n".to_string(), String::new()),
        "/broken" => ("500 Internal Server Error", String::new(), page("Oops", "<p>Server error page.</p>")),
        _ => ("404 Not Found", String::new(), page("Not found", "<p>Nothing here.</p>")),
    }
}

/// Run `ref fetch` with extra args and return (exit code, parsed JSON)
fn fetch(args: &[&str]) -> (i32, Value) {
    let output = cargo_bin_cmd!("ref")
        .arg("fetch")
        .args(args)
        .timeout(std::time::Duration::from_mins(1))
        .output()
        .expect("run ref");
    let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap_or(-1), json)
}

fn all_content(page: &Value) -> String {
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

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn http_404_is_dead_and_exits_1() {
    let base = serve();
    let (code, page) = fetch(&[&format!("{base}/missing")]);
    assert_eq!(page["status"], "dead");
    assert_eq!(page["http"], 404);
    assert_eq!(code, 1);
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn http_500_is_dead() {
    let base = serve();
    let (_, page) = fetch(&[&format!("{base}/broken")]);
    assert_eq!(page["status"], "dead");
    assert_eq!(page["http"], 500);
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn title_with_500_is_ok() {
    let base = serve();
    let (code, page) = fetch(&[&format!("{base}/fortune")]);
    assert_eq!(page["status"], "ok");
    assert_eq!(page["http"], 200);
    assert_eq!(code, 0);
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn redirect_on_same_host_is_followed() {
    let base = serve();
    let (_, page) = fetch(&[&format!("{base}/moved")]);
    assert_eq!(page["status"], "ok");
    assert_eq!(page["http"], 200);
    assert_eq!(page["title"], "Quarterly report");
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn lists_tables_quotes_and_loose_text_are_kept() {
    let base = serve();
    let (_, page) = fetch(&[&format!("{base}/report")]);
    let text = all_content(&page);
    for expected in [
        "Intro paragraph.",
        "- Revenue up 12%",
        "| Region | Q3 |\n| --- | --- |\n| North | 150 |",
        "> Strong quarter, said the CFO.",
        "Loose closing remark.",
    ] {
        assert!(text.contains(expected), "{expected:?} missing from {text}");
    }
    assert!(!text.contains("Home | Blog"), "nav kept: {text}");
    assert!(!text.contains("Site footer"), "footer kept: {text}");
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn long_pages_are_cut_with_a_flag_or_kept_whole() {
    let base = serve();
    let url = format!("{base}/long");

    let (_, page) = fetch(&[&url]);
    assert_eq!(page["truncated"], true);
    assert_eq!(page["sections"][0]["truncated"], true);
    assert!(page["alerts"].to_string().contains("--max-chars 0"));

    let (_, full) = fetch(&["--max-chars", "0", &url]);
    assert!(full.get("truncated").is_none());
    let len = full["sections"][0]["content"].as_str().unwrap_or("").len();
    assert!(len > 16_000, "only {len} characters kept");
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn content_loaded_by_fetch_after_load_is_seen() {
    let base = serve();
    let (_, page) = fetch(&[&format!("{base}/late")]);
    assert!(
        all_content(&page).contains("arrived through fetch()"),
        "{page}"
    );
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn walls_are_detected_only_when_real() {
    let base = serve();
    let (_, article) = fetch(&[&format!("{base}/paywall-article")]);
    assert_eq!(article["status"], "ok");

    let (_, login) = fetch(&[&format!("{base}/login")]);
    assert_eq!(login["status"], "login");
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn local_files_are_refused() {
    let (code, page) = fetch(&["file:///etc/hostname"]);
    assert_eq!(page["status"], "dead");
    assert!(page["alerts"].to_string().contains("Only http and https"));
    assert_eq!(code, 1);
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn check_links_reports_real_status_codes() {
    let base = serve();
    let output = cargo_bin_cmd!("ref")
        .args(["check-links", "--url", &format!("{base}/missing")])
        .timeout(std::time::Duration::from_mins(1))
        .output()
        .expect("run ref");
    let report: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(report["results"][0]["status"], 404);
    assert_eq!(report["failed"], 1);
    assert_eq!(output.status.code(), Some(1));
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn page_wrapped_in_a_form_keeps_its_content() {
    let base = serve();
    let (_, page) = fetch(&[&format!("{base}/aspnet")]);
    assert!(
        all_content(&page).contains("Revenue rose this year."),
        "{page}"
    );
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn empty_pages_and_unmatched_selectors_say_so() {
    let base = serve();
    let (_, empty) = fetch(&[&format!("{base}/empty")]);
    assert!(
        empty["alerts"].to_string().contains("No readable content"),
        "{empty}"
    );

    let (_, page) = fetch(&["--selector", "#missing", &format!("{base}/report")]);
    assert!(
        page["alerts"].to_string().contains("matched nothing"),
        "{page}"
    );
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn no_paywall_class_is_not_a_paywall() {
    let base = serve();
    let (_, page) = fetch(&[&format!("{base}/no-paywall")]);
    assert_eq!(page["status"], "ok");
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn shadow_dom_text_is_kept() {
    let base = serve();
    let (_, page) = fetch(&[&format!("{base}/components")]);
    assert!(
        all_content(&page).contains("Text that lives in a shadow root."),
        "{page}"
    );
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn hidden_overlay_is_not_a_paywall() {
    let base = serve();
    let (_, page) = fetch(&[&format!("{base}/hidden-overlay")]);
    assert_eq!(page["status"], "ok", "{page}");
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn signed_in_pdf_uses_the_cookie_file() {
    let base = serve();
    let dir = tempfile::tempdir().expect("tempdir");
    let jar = dir.path().join("cookies.txt");
    std::fs::write(&jar, "127.0.0.1\tFALSE\t/\tFALSE\t0\tsid\tletmein\n").expect("write jar");
    let url = format!("{base}/signed-in.pdf");

    let (code, page) = fetch(&["--cookies", jar.to_str().expect("path"), &url]);
    assert_eq!(page["status"], "ok", "{page}");
    assert!(all_content(&page).contains("signed-in readers"), "{page}");
    assert_eq!(code, 0);

    let (_, without) = fetch(&[&url]);
    assert_eq!(without["status"], "dead", "{without}");
}

#[test]
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn scan_refuses_to_overwrite_other_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("notes.md"), "See https://example.com/a").expect("write");
    std::fs::write(dir.path().join("main.rs"), "fn main() {}").expect("write");
    let out = cargo_bin_cmd!("ref")
        .current_dir(dir.path())
        .args(["scan", "notes.md", "--output", "main.rs", "--no-merge"])
        .output()
        .expect("run ref");
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("main.rs")).expect("read"),
        "fn main() {}"
    );
}
