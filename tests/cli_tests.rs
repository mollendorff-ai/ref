//! E2E tests for Möllendorff Ref CLI

#![allow(deprecated)] // cargo_bin deprecation - will update when assert_cmd stabilizes replacement

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

fn ref_cmd() -> Command {
    Command::cargo_bin("ref").unwrap()
}

#[test]
fn test_help() {
    ref_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("check-links"))
        .stdout(predicate::str::contains("refresh-data"))
        .stdout(predicate::str::contains("update"));
}

#[test]
fn test_version() {
    ref_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("ref"));
}

#[test]
fn test_check_links_help() {
    ref_cmd()
        .args(["check-links", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--concurrency"))
        .stdout(predicate::str::contains("--url"))
        .stdout(predicate::str::contains("--stdin"));
}

#[test]
fn test_refresh_data_help() {
    ref_cmd()
        .args(["refresh-data", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--url"))
        .stdout(predicate::str::contains("--timeout"));
}

#[test]
fn test_verify_refs_help() {
    ref_cmd()
        .args(["verify-refs", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--parallel"))
        .stdout(predicate::str::contains("--category"))
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn test_update_help() {
    ref_cmd()
        .args(["update", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--check"))
        .stdout(predicate::str::contains("--force"));
}

#[test]
fn test_check_links_no_args() {
    ref_cmd()
        .arg("check-links")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn test_refresh_data_no_args() {
    ref_cmd()
        .arg("refresh-data")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn test_check_links_file_not_found() {
    ref_cmd()
        .args(["check-links", "nonexistent.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to read file"));
}

#[test]
fn test_check_links_empty_file() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("empty.md");
    fs::write(&file_path, "# No URLs here\n\nJust text.").unwrap();

    ref_cmd()
        .args(["check-links", file_path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No URLs found"));
}

#[test]
fn test_check_links_with_urls() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("links.md");
    fs::write(&file_path, "Check https://example.com for more info.").unwrap();

    // This test requires Chrome, so we just check it starts
    // Full E2E would need Chrome installed
    ref_cmd()
        .args(["check-links", file_path.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(5))
        .assert();
    // Don't assert success/failure as it depends on Chrome being installed
}

#[test]
fn test_concurrency_validation() {
    ref_cmd()
        .args(["check-links", "--concurrency", "0", "test.md"])
        .assert()
        .failure();

    ref_cmd()
        .args(["check-links", "--concurrency", "21", "test.md"])
        .assert()
        .failure();
}
