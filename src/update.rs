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

/// # Errors
/// Returns an error if the latest release cannot be fetched or the binary cannot be replaced.
pub async fn run_update(args: UpdateArgs) -> Result<()> {
    eprintln!("Current version: {CURRENT_VERSION}");
    eprintln!("Checking for updates...");

    let release = fetch_latest_release().await?;
    let latest_version = release.tag_name.trim_start_matches('v');

    eprintln!("Latest version: {latest_version}");

    if latest_version == CURRENT_VERSION && !args.force {
        let output = serde_json::json!({
            "status": "up_to_date",
            "current_version": CURRENT_VERSION,
            "latest_version": latest_version
        });
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    if args.check {
        let output = serde_json::json!({
            "status": "update_available",
            "current_version": CURRENT_VERSION,
            "latest_version": latest_version
        });
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    // Determine target platform
    let target = get_target_triple()?;
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

    eprintln!("Downloading {}...", asset.name);

    // Download into a fresh private folder (not a predictable shared path)
    let temp_dir = env::temp_dir().join(format!("ref-update-{}", std::process::id()));
    fs::create_dir_all(&temp_dir).context("Failed to create temp folder")?;
    let archive_path = temp_dir.join(&asset.name);
    let archive = download_bytes(&asset.browser_download_url).await?;
    let sums = download_bytes(&checksums.browser_download_url).await?;

    let expected = expected_checksum(&String::from_utf8_lossy(&sums), &asset.name)
        .with_context(|| format!("checksums.txt has no entry for {}", asset.name))?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        let _ = fs::remove_dir_all(&temp_dir);
        bail!(
            "Checksum mismatch for {}: expected {expected}, got {actual}",
            asset.name
        );
    }
    eprintln!("Checksum verified (SHA-256).");
    fs::File::create(&archive_path)?.write_all(&archive)?;

    // Extract binary
    eprintln!("Extracting...");
    let binary_path = extract_binary(&archive_path, &temp_dir)?;

    // Get current binary path
    let current_exe = env::current_exe().context("Failed to get current executable path")?;

    // Replace current binary
    eprintln!("Installing to {}...", current_exe.display());
    install_binary(&binary_path, &current_exe)?;

    // Cleanup
    let _ = fs::remove_dir_all(&temp_dir);

    let output = serde_json::json!({
        "status": "updated",
        "previous_version": CURRENT_VERSION,
        "new_version": latest_version,
        "path": current_exe.display().to_string()
    });
    println!("{}", serde_json::to_string(&output)?);

    eprintln!("Updated successfully! Restart to use v{latest_version}");

    Ok(())
}

async fn fetch_latest_release() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");

    let client = reqwest::Client::builder()
        .user_agent("ref-update")
        .build()?;

    let response = client
        .get(&url)
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

async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .user_agent("ref-update")
        .build()?;

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

fn get_target_triple() -> Result<&'static str> {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;

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
    fn test_get_target_triple() {
        // Should return a valid triple for the current platform
        let triple = get_target_triple();
        assert!(triple.is_ok());
        let triple = triple.unwrap();
        assert!(
            triple.contains("linux") || triple.contains("darwin") || triple.contains("windows")
        );
    }
}
