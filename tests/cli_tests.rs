//! E2E tests for Möllendorff Ref CLI

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

fn ref_cmd() -> Command {
    cargo_bin_cmd!("ref")
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
#[ignore = "e2e: requires Chrome (ADR-003)"]
fn test_check_links_with_urls() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("links.md");
    fs::write(&file_path, "Check https://example.com for more info.").unwrap();

    ref_cmd()
        .args(["check-links", file_path.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success();
}

#[test]
fn test_mcp_subcommand_registered() {
    ref_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("mcp"));
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
