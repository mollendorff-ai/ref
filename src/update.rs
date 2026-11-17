//! update command: Self-update from GitHub releases
//!
//! Downloads the latest release binary from GitHub and replaces the current binary.

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Deserialize;
use std::env;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const GITHUB_REPO: &str = "mollendorff-ai/ref";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Args)]
pub struct UpdateArgs {
    /// Check for updates without installing
    #[arg(long)]
    pub check: bool,

    /// Force update even if already on latest version
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// How long `ref update` waits for GitHub, so a silent server cannot hang it
#[derive(Clone, Copy)]
struct Limits {
    /// To open a connection
    connect: Duration,
    /// For the whole request for the release description (a small JSON file)
    release: Duration,
    /// For the whole download of one file (the archive is a few megabytes)
    download: Duration,
}

const GITHUB_LIMITS: Limits = Limits {
    connect: Duration::from_secs(15),
    release: Duration::from_secs(30),
    download: Duration::from_mins(5),
};

/// Where an update comes from and where it goes: GitHub and the running
/// binary, or, in tests, a fake GitHub and a scratch folder.
struct Source {
    latest_release_url: String,
    exe: PathBuf,
    temp_root: PathBuf,
    limits: Limits,
}

/// # Errors
/// Returns an error if the latest release cannot be fetched or the binary cannot be replaced.
pub async fn run_update(args: UpdateArgs, out: &mut dyn std::io::Write) -> Result<()> {
    let source = Source {
        latest_release_url: format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest"),
        exe: env::current_exe().context("Failed to get current executable path")?,
        temp_root: env::temp_dir(),
        limits: GITHUB_LIMITS,
    };
    update_from(&source, &args, out).await
}

async fn update_from(source: &Source, args: &UpdateArgs, out: &mut dyn Write) -> Result<()> {
    eprintln!("Current version: {CURRENT_VERSION}");
    eprintln!("Checking for updates...");

    let release = fetch_latest_release(&source.latest_release_url, &source.limits).await?;
    let latest_version = release.tag_name.trim_start_matches('v');

    eprintln!("Latest version: {latest_version}");

    if latest_version == CURRENT_VERSION && !args.force {
        let output = serde_json::json!({
            "status": "up_to_date",
            "current_version": CURRENT_VERSION,
            "latest_version": latest_version
        });
        writeln!(out, "{output}")?;
        return Ok(());
    }

    if args.check {
        let output = serde_json::json!({
            "status": "update_available",
            "current_version": CURRENT_VERSION,
            "latest_version": latest_version
        });
        writeln!(out, "{output}")?;
        return Ok(());
    }

    let target = target_triple(env::consts::OS, env::consts::ARCH)?;
    eprintln!("Platform: {target}");

    // Find matching asset (.zip for Windows, .tar.gz for Unix)
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let asset_name = format!("ref-{target}.{ext}");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .with_context(|| format!("No release found for platform: {target}"))?;

    let checksums = release
        .assets
        .iter()
        .find(|a| a.name == "checksums.txt")
        .context("Release has no checksums.txt; refusing to install an unverified binary")?;

    // Work in a fresh private folder, removed however the install ends
    let temp_dir = source
        .temp_root
        .join(format!("ref-update-{}", std::process::id()));
    fs::create_dir_all(&temp_dir).context("Failed to create temp folder")?;
    let installed =
        download_and_install(asset, checksums, &temp_dir, &source.exe, &source.limits).await;
    let _ = fs::remove_dir_all(&temp_dir);
    installed?;

    let output = serde_json::json!({
        "status": "updated",
        "previous_version": CURRENT_VERSION,
        "new_version": latest_version,
        "path": source.exe.display().to_string()
    });
    writeln!(out, "{output}")?;

    eprintln!("Updated successfully! Restart to use v{latest_version}");

    Ok(())
}

/// Download `asset`, check it against `checksums`, and put its binary at `exe`.
async fn download_and_install(
    asset: &Asset,
    checksums: &Asset,
    temp_dir: &Path,
    exe: &Path,
    limits: &Limits,
) -> Result<()> {
    eprintln!("Downloading {}...", asset.name);
    let archive = download_bytes(&asset.browser_download_url, limits).await?;
    let sums = download_bytes(&checksums.browser_download_url, limits).await?;

    let expected = expected_checksum(&String::from_utf8_lossy(&sums), &asset.name)
        .with_context(|| format!("checksums.txt has no entry for {}", asset.name))?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        bail!(
            "Checksum mismatch for {}: expected {expected}, got {actual}",
            asset.name
        );
    }
    eprintln!("Checksum verified (SHA-256).");
    let archive_path = temp_dir.join(&asset.name);
    fs::write(&archive_path, &archive)?;

    eprintln!("Extracting...");
    let binary_path = extract_binary(&archive_path, temp_dir)?;

    eprintln!("Installing to {}...", exe.display());
    install_binary(&binary_path, exe)
}

/// One client for every request of `ref update`: it names itself and gives up
/// after `connect` to open a connection or `total` for the whole request.
fn client(connect: Duration, total: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("ref-update")
        .connect_timeout(connect)
        .timeout(total)
        .build()
        .context("Failed to create the HTTP client")
}

async fn fetch_latest_release(url: &str, limits: &Limits) -> Result<Release> {
    let client = client(limits.connect, limits.release)?;

    let response = client
        .get(url)
        .send()
        .await
        .context("Failed to fetch release info")?;

    if !response.status().is_success() {
        bail!(
            "GitHub API error: {} - {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
    }

    let release: Release = response.json().await.context("Failed to parse release")?;
    Ok(release)
}

async fn download_bytes(url: &str, limits: &Limits) -> Result<Vec<u8>> {
    let client = client(limits.connect, limits.download)?;

    let response = client.get(url).send().await.context("Failed to download")?;

    if !response.status().is_success() {
        bail!("Download failed: {}", response.status());
    }

    Ok(response.bytes().await?.to_vec())
}

/// Find the hash for `file` in a `sha256sum`-style listing (`<hex>  <name>`).
fn expected_checksum(listing: &str, file: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == file).then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn extract_binary(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf> {
    use std::process::Command;

    let output = Command::new("tar")
        .args([
            "-xzf",
            archive_path.to_str().unwrap(),
            "-C",
            dest_dir.to_str().unwrap(),
            "ref",
        ])
        .output()
        .context("Failed to extract archive")?;

    if !output.status.success() {
        bail!(
            "Extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(dest_dir.join("ref"))
}

fn install_binary(src: &Path, dest: &Path) -> Result<()> {
    // Backup current binary
    let backup = dest.with_extension("old");
    if dest.exists() {
        fs::rename(dest, &backup).context("Failed to backup current binary")?;
    }

    // Copy new binary
    match fs::copy(src, dest) {
        Ok(_) => {
            // Set executable permissions (Unix only)
            #[cfg(unix)]
            {
                let mut perms = fs::metadata(dest)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(dest, perms)?;
            }

            // Remove backup
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(e) => {
            // Restore backup on failure
            if backup.exists() {
                let _ = fs::rename(&backup, dest);
            }
            Err(e.into())
        }
    }
}

/// The release build for an OS and architecture (`std::env::consts` names).
fn target_triple(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => bail!("Unsupported platform: {os}-{arch}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expected_checksum() {
        let listing = "AABB11  ref-x86_64-unknown-linux-musl.tar.gz\nccdd22 *ref-aarch64-apple-darwin.tar.gz\n\n";
        assert_eq!(
            expected_checksum(listing, "ref-x86_64-unknown-linux-musl.tar.gz").as_deref(),
            Some("aabb11")
        );
        assert_eq!(
            expected_checksum(listing, "ref-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("ccdd22")
        );
        assert_eq!(
            expected_checksum(listing, "ref-x86_64-pc-windows-msvc.zip"),
            None
        );
    }

    #[test]
    fn test_sha256_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn each_release_platform_has_a_build() {
        let builds = [
            ("linux", "x86_64", "x86_64-unknown-linux-musl"),
            ("linux", "aarch64", "aarch64-unknown-linux-musl"),
            ("macos", "x86_64", "x86_64-apple-darwin"),
            ("macos", "aarch64", "aarch64-apple-darwin"),
            ("windows", "x86_64", "x86_64-pc-windows-msvc"),
        ];
        for (os, arch, triple) in builds {
            assert_eq!(target_triple(os, arch).unwrap(), triple);
        }
        let other = target_triple("freebsd", "x86_64").unwrap_err();
        assert_eq!(other.to_string(), "Unsupported platform: freebsd-x86_64");
    }

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// This platform's release archive
    fn archive_name() -> String {
        let target = target_triple(env::consts::OS, env::consts::ARCH).unwrap();
        format!("ref-{target}.tar.gz")
    }

    /// A release archive holding a `ref` binary with `contents`
    fn archive_of(contents: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ref"), contents).unwrap();
        let archive = dir.path().join("a.tar.gz");
        let made = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.path())
            .arg("ref")
            .status()
            .unwrap();
        assert!(made.success());
        fs::read(archive).unwrap()
    }

    /// A fake GitHub whose latest release is `tag`, with these assets
    async fn github(tag: &str, assets: &[(&str, Vec<u8>)]) -> MockServer {
        let server = MockServer::start().await;
        let listed: Vec<_> = assets
            .iter()
            .map(|(name, _)| {
                serde_json::json!({
                    "name": name,
                    "browser_download_url": format!("{}/download/{name}", server.uri()),
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"tag_name": tag, "assets": listed})),
            )
            .mount(&server)
            .await;
        for (name, body) in assets {
            Mock::given(method("GET"))
                .and(path(format!("/download/{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
                .mount(&server)
                .await;
        }
        server
    }

    /// A newer release with this platform's archive and a checksum listing
    async fn newer_release(archive: Vec<u8>, sums: &str) -> MockServer {
        github(
            "v99.0.0",
            &[
                (&archive_name(), archive),
                ("checksums.txt", sums.as_bytes().to_vec()),
            ],
        )
        .await
    }

    /// The listing that matches `archive`
    fn sums_of(archive: &[u8]) -> String {
        format!("{}  {}\n", sha256_hex(archive), archive_name())
    }

    struct Run {
        dir: tempfile::TempDir,
        exe: PathBuf,
    }

    /// An installed `ref` (contents "old") in a scratch folder
    fn installed() -> Run {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("ref");
        fs::write(&exe, "old").unwrap();
        Run { dir, exe }
    }

    impl Run {
        async fn update(&self, url: &str, check: bool, force: bool) -> Result<serde_json::Value> {
            self.update_within(url, check, force, GITHUB_LIMITS).await
        }

        async fn update_within(
            &self,
            url: &str,
            check: bool,
            force: bool,
            limits: Limits,
        ) -> Result<serde_json::Value> {
            let source = Source {
                latest_release_url: url.to_string(),
                exe: self.exe.clone(),
                temp_root: self.dir.path().to_path_buf(),
                limits,
            };
            let mut out = Vec::new();
            update_from(&source, &UpdateArgs { check, force }, &mut out).await?;
            Ok(serde_json::from_slice(&out).unwrap())
        }

        /// Nothing left in the scratch folder but the binary
        fn only_the_binary_is_left(&self) {
            let names: Vec<_> = fs::read_dir(self.dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, ["ref"]);
        }

        fn binary(&self) -> String {
            fs::read_to_string(&self.exe).unwrap()
        }
    }

    #[tokio::test]
    async fn the_latest_version_is_up_to_date() {
        let server = github(&format!("v{CURRENT_VERSION}"), &[]).await;
        let run = installed();
        let out = run
            .update(&format!("{}/latest", server.uri()), false, false)
            .await;
        let out = out.unwrap();
        assert_eq!(out["status"], "up_to_date");
        assert_eq!(out["latest_version"], CURRENT_VERSION);
        assert_eq!(run.binary(), "old");
    }

    #[tokio::test]
    async fn check_reports_a_newer_version_without_installing_it() {
        let server = github("v99.0.0", &[]).await;
        let run = installed();
        let out = run
            .update(&format!("{}/latest", server.uri()), true, false)
            .await;
        let out = out.unwrap();
        assert_eq!(out["status"], "update_available");
        assert_eq!(out["current_version"], CURRENT_VERSION);
        assert_eq!(out["latest_version"], "99.0.0");
        assert_eq!(run.binary(), "old");
    }

    #[tokio::test]
    async fn a_newer_version_replaces_the_binary() {
        let archive = archive_of("new");
        let server = newer_release(archive.clone(), &sums_of(&archive)).await;
        let run = installed();
        let out = run
            .update(&format!("{}/latest", server.uri()), false, false)
            .await;
        let out = out.unwrap();
        assert_eq!(out["status"], "updated");
        assert_eq!(out["new_version"], "99.0.0");
        assert_eq!(out["path"], run.exe.display().to_string());
        assert_eq!(run.binary(), "new");
        let mode = fs::metadata(&run.exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        run.only_the_binary_is_left();
    }

    #[tokio::test]
    async fn force_reinstalls_the_same_version() {
        let archive = archive_of("again");
        let sums = sums_of(&archive);
        let server = github(
            &format!("v{CURRENT_VERSION}"),
            &[
                (&archive_name(), archive),
                ("checksums.txt", sums.into_bytes()),
            ],
        )
        .await;
        let run = installed();
        let out = run
            .update(&format!("{}/latest", server.uri()), false, true)
            .await;
        assert_eq!(out.unwrap()["status"], "updated");
        assert_eq!(run.binary(), "again");
    }

    /// What a failed update says; the binary and scratch folder are untouched
    async fn refused(server: &MockServer) -> String {
        let run = installed();
        let url = format!("{}/latest", server.uri());
        let err = run.update(&url, false, false).await.unwrap_err();
        assert_eq!(run.binary(), "old");
        run.only_the_binary_is_left();
        format!("{err:#}")
    }

    #[tokio::test]
    async fn a_release_without_this_platform_is_refused() {
        let server = github("v99.0.0", &[("ref-other.tar.gz", Vec::new())]).await;
        let err = refused(&server).await;
        assert!(err.starts_with("No release found for platform"), "{err}");
    }

    #[tokio::test]
    async fn a_release_without_checksums_is_refused() {
        let server = github("v99.0.0", &[(&archive_name(), archive_of("new"))]).await;
        let err = refused(&server).await;
        assert!(err.contains("no checksums.txt"), "{err}");
    }

    #[tokio::test]
    async fn an_archive_missing_from_the_checksums_is_refused() {
        let server = newer_release(archive_of("new"), "abc  ref-other.tar.gz\n").await;
        let err = refused(&server).await;
        assert!(err.contains("checksums.txt has no entry for"), "{err}");
    }

    #[tokio::test]
    async fn an_archive_with_the_wrong_checksum_is_refused() {
        let server = newer_release(archive_of("tampered"), &sums_of(b"genuine")).await;
        let err = refused(&server).await;
        assert!(err.starts_with("Checksum mismatch for"), "{err}");
    }

    #[tokio::test]
    async fn an_archive_that_is_not_one_is_refused() {
        let junk = b"not a tar file".to_vec();
        let server = newer_release(junk.clone(), &sums_of(&junk)).await;
        let err = refused(&server).await;
        assert!(err.starts_with("Extraction failed"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_download_is_refused() {
        // Both files are listed, neither is served
        let server = MockServer::start().await;
        let gone = format!("{}/gone", server.uri());
        let release = serde_json::json!({
            "tag_name": "v99.0.0",
            "assets": [
                {"name": archive_name(), "browser_download_url": gone},
                {"name": "checksums.txt", "browser_download_url": gone},
            ],
        });
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release))
            .mount(&server)
            .await;
        let err = refused(&server).await;
        assert_eq!(err, "Download failed: 404 Not Found");
    }

    /// Limits short enough for a test; the server answers after 30 s
    const QUICK: Limits = Limits {
        connect: Duration::from_secs(5),
        release: Duration::from_millis(300),
        download: Duration::from_millis(300),
    };
    const SILENCE: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn a_github_that_never_answers_with_the_release_is_given_up_on() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(ResponseTemplate::new(200).set_delay(SILENCE))
            .mount(&server)
            .await;
        let run = installed();
        let started = std::time::Instant::now();
        let url = format!("{}/latest", server.uri());
        let err = run.update_within(&url, true, false, QUICK).await;
        let err = format!("{:#}", err.unwrap_err());
        assert!(err.starts_with("Failed to fetch release info"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10), "{err}");
    }

    #[tokio::test]
    async fn a_download_that_never_arrives_is_given_up_on() {
        let server = MockServer::start().await;
        let slow = format!("{}/slow", server.uri());
        let release = serde_json::json!({
            "tag_name": "v99.0.0",
            "assets": [
                {"name": archive_name(), "browser_download_url": slow},
                {"name": "checksums.txt", "browser_download_url": slow},
            ],
        });
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(SILENCE))
            .mount(&server)
            .await;
        let run = installed();
        let started = std::time::Instant::now();
        let url = format!("{}/latest", server.uri());
        let err = run.update_within(&url, false, false, QUICK).await;
        let err = format!("{:#}", err.unwrap_err());
        assert!(err.starts_with("Failed to download"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10), "{err}");
        run.only_the_binary_is_left();
    }

    #[tokio::test]
    async fn github_errors_are_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(ResponseTemplate::new(403).set_body_string("rate limited"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/garbled"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{"))
            .mount(&server)
            .await;
        let err = refused(&server).await;
        assert_eq!(err, "GitHub API error: 403 Forbidden - rate limited");

        let run = installed();
        let garbled = run
            .update(&format!("{}/garbled", server.uri()), true, false)
            .await;
        let garbled = format!("{:#}", garbled.unwrap_err());
        assert!(garbled.starts_with("Failed to parse release"), "{garbled}");

        // Nothing listening
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let offline = run
            .update(&format!("http://127.0.0.1:{port}/latest"), true, false)
            .await;
        let offline = format!("{:#}", offline.unwrap_err());
        assert!(
            offline.starts_with("Failed to fetch release info"),
            "{offline}"
        );
    }

    #[test]
    fn a_failed_install_puts_the_old_binary_back() {
        let run = installed();
        let missing = run.dir.path().join("missing");
        assert!(install_binary(&missing, &run.exe).is_err());
        assert_eq!(run.binary(), "old");
        run.only_the_binary_is_left();

        // A first install has nothing to back up
        let fresh = run.dir.path().join("fresh");
        install_binary(&run.exe, &fresh).unwrap();
        assert_eq!(fs::read_to_string(fresh).unwrap(), "old");
    }
}
