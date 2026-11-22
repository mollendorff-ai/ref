//! The proxy ref sends its traffic through, when you ask for one.
//!
//! `--proxy <URL>` (or `REF_PROXY`) sends everything a command fetches through
//! one proxy: the pages Chrome loads and the PDFs ref downloads itself.
//! The flag wins over the variable, and both win over the `http_proxy` family
//! of variables that Chrome and the downloads read on their own.
//!
//! The address is `scheme://host:port` with the scheme `http`, `https`,
//! `socks5` or `socks5h` (a SOCKS5 proxy always looks names up itself, so the
//! two mean the same), and no user name or password: Chrome cannot take
//! credentials on its command line, and a password there is visible to every
//! user of the machine in the process list.
//! The port is always spelled out in [`Proxy::as_str`], so one proxy has one
//! form.

use url::Url;

/// Environment variable that gives the proxy when `--proxy` is not given
pub const PROXY_ENV: &str = "REF_PROXY";

/// The addresses on this machine, which Chrome never sends through a proxy;
/// the downloads leave them out too, so a page and its PDF go the same way
const LOCAL_ADDRESSES: &str = "localhost,127.0.0.1,::1";

/// What a refused address says to do instead
const ADDRESS_FORM: &str = "Use scheme://host:port with http, https, socks5 or socks5h, \
     for example socks5://127.0.0.1:1080";

/// A checked proxy address, in its one canonical form `scheme://host:port`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    url: String,
}

impl Proxy {
    /// A proxy that skipped the checks, to see what a failure to use one does
    #[cfg(test)]
    pub(crate) fn unchecked(url: &str) -> Self {
        Self {
            url: url.to_string(),
        }
    }

    /// The address, as `scheme://host:port`
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.url
    }

    /// The address for Chrome's `--proxy-server`. Chrome does not know
    /// `socks5h`: it always asks a SOCKS5 proxy to look names up, which is
    /// what `socks5h` means.
    #[must_use]
    pub fn chrome_server(&self) -> String {
        self.url.replacen("socks5h://", "socks5://", 1)
    }

    /// The address for a download made outside Chrome. The download library
    /// looks names up on this machine for `socks5` and in the proxy for
    /// `socks5h`; ref asks the proxy for both, as Chrome does, so one proxy
    /// sees the same names from every part of ref and none leave this machine.
    #[must_use]
    pub fn download_server(&self) -> String {
        self.url.replacen("socks5://", "socks5h://", 1)
    }

    /// The proxy for a download made outside Chrome, for every address but
    /// those on this machine ([`LOCAL_ADDRESSES`])
    ///
    /// # Errors
    /// Returns an error if the download library cannot use the address.
    pub fn for_download(&self) -> Result<reqwest::Proxy, reqwest::Error> {
        let proxy = reqwest::Proxy::all(self.download_server())?;
        Ok(proxy.no_proxy(reqwest::NoProxy::from_string(LOCAL_ADDRESSES)))
    }
}

/// Parse a `--proxy` value.
///
/// # Errors
/// Returns what is wrong with the address: not an address, another scheme,
/// credentials, a path, query or fragment, or no port to use.
pub fn parse_proxy(value: &str) -> Result<Proxy, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("the proxy is empty. {ADDRESS_FORM}"));
    }
    let url = Url::parse(value)
        .map_err(|e| format!("{value} is not an address ({e}). {ADDRESS_FORM}"))?;
    if !["http", "https", "socks5", "socks5h"].contains(&url.scheme()) {
        return Err(format!(
            "{value}: the scheme {:?} is not supported. {ADDRESS_FORM}",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "{}: a proxy with a user name or password is not supported yet \
             (Chrome cannot take them on its command line). Use a proxy that \
             lets this machine in without them, for example an SSH tunnel.",
            url.scheme()
        ));
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "{value}: a proxy is a host and a port, nothing after them. {ADDRESS_FORM}"
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("{value} has no host. {ADDRESS_FORM}"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| format!("{value} has no port. {ADDRESS_FORM}"))?;
    Ok(Proxy {
        url: format!("{}://{host}:{port}", url.scheme()),
    })
}

/// The proxy to use: `flag` when given, else `REF_PROXY` when it is set and
/// not empty (an empty value counts as not set).
///
/// # Errors
/// Returns an error when the variable holds an address that cannot be used.
pub fn resolve(flag: Option<&Proxy>) -> anyhow::Result<Option<Proxy>> {
    resolve_from(flag, std::env::var(PROXY_ENV).ok().as_deref())
}

/// [`resolve`] with the variable's value given
fn resolve_from(flag: Option<&Proxy>, from_env: Option<&str>) -> anyhow::Result<Option<Proxy>> {
    if let Some(proxy) = flag {
        return Ok(Some(proxy.clone()));
    }
    match from_env.map(str::trim) {
        Some(value) if !value.is_empty() => parse_proxy(value)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{PROXY_ENV}: {e}")),
        _ => Ok(None),
    }
}

/// The `--proxy` option of the commands that fetch
#[derive(clap::Args, Debug, Clone, Default)]
pub struct ProxyArgs {
    /// Send everything through this proxy: scheme://host:port with http,
    /// https, socks5 or socks5h, and no user name or password
    /// [default: `REF_PROXY` when set, else none]
    #[arg(long, value_name = "URL", value_parser = parse_proxy)]
    pub proxy: Option<Proxy>,
}

impl ProxyArgs {
    /// The `--proxy` value, else `REF_PROXY` (see [`resolve`])
    ///
    /// # Errors
    /// Returns an error when the variable holds an address that cannot be used.
    pub fn choice(&self) -> anyhow::Result<Option<Proxy>> {
        resolve(self.proxy.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct WithProxy {
        #[command(flatten)]
        proxy: ProxyArgs,
    }

    fn proxy(value: &str) -> Proxy {
        parse_proxy(value).unwrap()
    }

    #[test]
    fn every_supported_scheme_is_kept_with_its_port_spelled_out() {
        for (given, canonical) in [
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
            (
                "  http://proxy.example.com  ",
                "http://proxy.example.com:80",
            ),
            ("https://proxy.example.com", "https://proxy.example.com:443"),
            (
                "HTTP://Proxy.Example.com:3128/",
                "http://proxy.example.com:3128",
            ),
            ("socks5://localhost:1080", "socks5://localhost:1080"),
            ("socks5h://[::1]:9050", "socks5h://[::1]:9050"),
        ] {
            assert_eq!(proxy(given).as_str(), canonical, "{given}");
        }
    }

    #[test]
    fn chrome_gets_socks5_for_socks5h_and_the_rest_as_given() {
        assert_eq!(proxy("socks5h://h:9050").chrome_server(), "socks5://h:9050");
        assert_eq!(proxy("socks5://h:1080").chrome_server(), "socks5://h:1080");
        assert_eq!(proxy("http://h:8080").chrome_server(), "http://h:8080");
    }

    #[test]
    fn a_socks5_proxy_looks_names_up_for_the_downloads_too() {
        assert_eq!(
            proxy("socks5://h:1080").download_server(),
            "socks5h://h:1080"
        );
        assert_eq!(
            proxy("socks5h://h:1080").download_server(),
            "socks5h://h:1080"
        );
        assert_eq!(proxy("https://h:8443").download_server(), "https://h:8443");
    }

    #[test]
    fn what_cannot_be_a_proxy_is_refused_with_what_to_do() {
        for (given, says) in [
            ("", "is empty"),
            ("   ", "is empty"),
            ("localhost", "is not an address"),
            ("127.0.0.1:8080", "is not an address"),
            ("localhost:8080", "scheme"),
            ("ftp://h:21", "scheme"),
            ("socks4://h:1080", "scheme"),
            ("http://user:secret@h:8080", "user name or password"),
            ("http://user@h:8080", "user name or password"),
            ("socks5://h:1080/path", "nothing after"),
            ("http://h:8080?x=1", "nothing after"),
            ("http://h:8080#top", "nothing after"),
            ("socks5://h", "no port"),
            ("socks5://", "has no host"),
            ("http://", "is not an address"),
        ] {
            let error = parse_proxy(given).unwrap_err();
            assert!(error.contains(says), "{given}: {error}");
        }
        // A password is never repeated in the message
        let refused = parse_proxy("http://user:secret@h:8080").unwrap_err();
        assert!(!refused.contains("secret"), "{refused}");
        assert!(refused.contains("SSH tunnel"), "{refused}");
    }

    #[test]
    fn the_option_beats_the_variable_and_an_empty_variable_is_not_set() {
        let flag = proxy("http://flag:1");
        let given = resolve_from(Some(&flag), Some("http://env:2")).unwrap();
        assert_eq!(given, Some(flag));
        let from_env = resolve_from(None, Some(" socks5h://env:2 ")).unwrap();
        assert_eq!(from_env, Some(proxy("socks5h://env:2")));
        assert_eq!(resolve_from(None, Some("  ")).unwrap(), None);
        assert_eq!(resolve_from(None, None).unwrap(), None);
        let bad = resolve_from(None, Some("env:2")).unwrap_err().to_string();
        assert!(bad.starts_with("REF_PROXY: "), "{bad}");
    }

    #[test]
    fn the_option_is_read_from_the_environment_only_when_absent() {
        // With the option given the variable is never looked at, so the answer
        // does not depend on this machine's environment
        let flag = proxy("http://flag:1");
        assert_eq!(resolve(Some(&flag)).unwrap(), Some(flag.clone()));
        let args = WithProxy::try_parse_from(["ref", "--proxy", "socks5h://h:9050"]).unwrap();
        assert_eq!(args.proxy.proxy, Some(proxy("socks5h://h:9050")));
        assert_eq!(
            args.proxy.choice().unwrap(),
            Some(proxy("socks5h://h:9050"))
        );
        let refused = WithProxy::try_parse_from(["ref", "--proxy", "h:9"])
            .err()
            .unwrap();
        assert!(refused.to_string().contains("scheme"), "{refused}");
    }

    use crate::test_server::{all_content, fetch, ref_cmd, serve, serve_socks5};
    use std::process::ExitCode;

    /// A host name that does not exist: a page that loads from it has come
    /// through the proxy, which answers for every name
    const NOWHERE: &str = "http://nowhere.example.test";

    /// The test server as an HTTP proxy, and as SOCKS5 proxies (the second for
    /// `socks5h`, the first for `socks5`)
    fn proxies() -> [String; 3] {
        let web = serve();
        let socks = serve_socks5(&web);
        [
            web,
            format!("socks5://{socks}"),
            format!("socks5h://{socks}"),
        ]
    }

    #[tokio::test]
    async fn fetch_loads_pages_through_each_kind_of_proxy() {
        // Needs Chrome (ADR-004)
        for proxy in proxies() {
            let (code, page) = fetch(&["--proxy", &proxy, &format!("{NOWHERE}/report")]).await;
            assert_eq!(page["status"], "ok", "{proxy}: {page}");
            assert_eq!(page["title"], "Quarterly report", "{proxy}");
            assert!(all_content(&page).contains("Revenue up 12%"), "{proxy}");
            assert_eq!(code, ExitCode::SUCCESS, "{proxy}");
        }
    }

    #[tokio::test]
    async fn a_pdf_found_by_fetch_is_downloaded_through_the_proxy_too() {
        // Needs Chrome (ADR-004): Chrome shows the PDF, then ref downloads it
        // itself, and that download must not leave directly (it could not: the
        // host name does not exist)
        for proxy in proxies() {
            let (_, page) = fetch(&["--proxy", &proxy, &format!("{NOWHERE}/report.pdf")]).await;
            assert_eq!(page["status"], "ok", "{proxy}: {page}");
            assert!(
                page["alerts"].to_string().contains("PDF detected"),
                "{page}"
            );
        }
    }

    #[tokio::test]
    async fn the_pdf_command_downloads_through_the_proxy() {
        for proxy in proxies() {
            let url = format!("{NOWHERE}/report.pdf");
            let (code, out, err) = ref_cmd(&["pdf", "--proxy", &proxy, &url]).await;
            assert_eq!(code, ExitCode::SUCCESS, "{proxy}: {out} {err}");
            assert!(out.contains("\"status\":\"ok\""), "{proxy}: {out}");
        }
    }

    #[tokio::test]
    async fn the_other_commands_go_through_the_proxy_too() {
        // Needs Chrome (ADR-004)
        let [proxy, ..] = proxies();
        let url = format!("{NOWHERE}/report");

        let line = [
            "check-links",
            "--profile",
            "fresh",
            "--proxy",
            &proxy,
            "--url",
            &url,
        ];
        let (code, out, err) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::SUCCESS, "{out} {err}");
        let report: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["results"][0]["status"], 200, "{report}");

        let line = [
            "refresh-data",
            "--profile",
            "fresh",
            "--proxy",
            &proxy,
            "--url",
            &url,
        ];
        let (code, out, err) = ref_cmd(&line).await;
        assert_eq!(code, ExitCode::SUCCESS, "{out} {err}");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("references.yaml");
        crate::verify_refs::tests::write_reference(&file, &url);
        let line = ["verify-refs", "--profile", "fresh", "--proxy", &proxy];
        let (code, out, err) = ref_cmd(&[&line[..], &[file.to_str().unwrap()]].concat()).await;
        assert_eq!(code, ExitCode::SUCCESS, "{out} {err}");
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("status: ok"), "{written}");
    }

    #[tokio::test]
    async fn addresses_on_this_machine_never_go_through_the_proxy() {
        // Needs Chrome (ADR-004). Nothing listens on port 1, so a request that
        // went to the proxy would fail. Chrome keeps these addresses off the
        // proxy by itself; the PDF download is told to.
        let base = serve();
        let none = "http://127.0.0.1:1";
        let (_, page) = fetch(&["--proxy", none, &format!("{base}/report")]).await;
        assert_eq!(page["status"], "ok", "{page}");
        let (_, pdf) = fetch(&["--proxy", none, &format!("{base}/report.pdf")]).await;
        assert_eq!(pdf["status"], "ok", "{pdf}");
        let (code, out, err) =
            ref_cmd(&["pdf", "--proxy", none, &format!("{base}/report.pdf")]).await;
        assert_eq!(code, ExitCode::SUCCESS, "{out} {err}");
    }

    #[tokio::test]
    async fn a_proxy_that_is_not_there_is_named_and_not_taken_for_the_site() {
        // Needs Chrome (ADR-004). Nothing listens on port 1.
        let (code, page) = fetch(&[
            "--proxy",
            "http://127.0.0.1:1",
            &format!("{NOWHERE}/report"),
        ])
        .await;
        assert_eq!(page["status"], "dead", "{page}");
        let alert = page["alerts"][0].as_str().unwrap();
        assert!(alert.starts_with("PROXY_FAILED: "), "{alert}");
        assert!(alert.ends_with("(proxy http://127.0.0.1:1)"), "{alert}");
        assert_eq!(code, ExitCode::from(1));
    }

    /// Set in the child process `the_variable_sends_everything_through_the_proxy` starts
    const CHILD: &str = "REF_TEST_PROXY_CHILD";

    #[tokio::test]
    async fn the_variable_sends_everything_through_the_proxy() {
        // Runs again as a child process with REF_PROXY set: setting it in this
        // process would race the other tests
        match std::env::var(CHILD).as_deref() {
            Ok("page") => {
                let (_, page) = fetch(&[&format!("{NOWHERE}/report")]).await;
                assert_eq!(page["status"], "ok", "{page}");
                let (code, out, _) = ref_cmd(&["pdf", &format!("{NOWHERE}/report.pdf")]).await;
                assert_eq!(code, ExitCode::SUCCESS, "{out}");
                // The option still wins over the variable
                let (_, wins) = fetch(&[
                    "--proxy",
                    "http://127.0.0.1:1",
                    &format!("{NOWHERE}/report"),
                ])
                .await;
                assert_eq!(wins["status"], "dead", "{wins}");
            }
            Ok(_) => {
                let (code, _, err) =
                    ref_cmd(&["fetch", "--profile", "fresh", "http://127.0.0.1:9/"]).await;
                assert_eq!(code, ExitCode::from(2));
                assert!(err.starts_with("Error: REF_PROXY: "), "{err}");
            }
            Err(_) => {
                for (mode, value) in [("page", serve()), ("refused", "not a proxy".to_string())] {
                    let child = std::process::Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "proxy::tests::the_variable_sends_everything_through_the_proxy",
                        ])
                        .env(CHILD, mode)
                        .env(PROXY_ENV, value)
                        .output()
                        .unwrap();
                    let shown = String::from_utf8_lossy(&child.stdout);
                    assert!(child.status.success(), "child run ({mode}) failed: {shown}");
                }
            }
        }
    }

    #[test]
    fn an_address_the_download_library_refuses_is_an_error_not_a_panic() {
        assert!(Proxy::unchecked("http://not a host:1")
            .for_download()
            .is_err());
    }

    #[test]
    fn a_checked_proxy_is_accepted_by_the_download_library() {
        for given in [
            "http://h:8080",
            "https://h:8443",
            "socks5://h:1080",
            "socks5h://h:9050",
        ] {
            assert!(proxy(given).for_download().is_ok(), "{given}");
        }
    }
}
