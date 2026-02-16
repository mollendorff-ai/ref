//! init command: Create references.yaml template
//!
//! Creates a new references.yaml file with proper structure.

use crate::schema::{Meta, Reference, ReferencesFile, Status};
use anyhow::{bail, Result};
use chrono::Utc;
use clap::Args;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Args)]
pub struct InitArgs {
    /// Output file path (default: references.yaml)
    #[arg(short, long, default_value = "references.yaml")]
    pub output: PathBuf,

    /// Overwrite existing file
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
pub struct InitOutput {
    pub created: String,
    pub file: String,
}

/// # Errors
/// Returns an error if the output file already exists (without `--force`) or cannot be written.
pub async fn run_init(args: InitArgs) -> Result<()> {
    // Check if file exists
    if args.output.exists() && !args.force {
        let error = serde_json::json!({
            "error": "file_exists",
            "message": format!("{} already exists. Use --force to overwrite.", args.output.display()),
            "file": args.output.display().to_string()
        });
        println!("{}", serde_json::to_string(&error)?);
        bail!("File exists");
    }

    let now = Utc::now();
    let date = now.format("%Y-%m-%d").to_string();

    // Create template with one example reference
    let refs_file = ReferencesFile {
        meta: Meta {
            created: date,
            last_verified: None,
            tool: "ref".to_string(),
            total_links: 1,
        },
        references: vec![Reference {
            url: "https://example.com".to_string(),
            title: "Example Reference".to_string(),
            categories: vec!["example".to_string()],
            cited_in: vec!["README.md".to_string()],
            status: Status::Pending,
            verified: None,
            notes: None,
        }],
    };

    // Serialize to YAML
    let yaml = serde_yaml::to_string(&refs_file)?;

    // Write file
    tokio::fs::write(&args.output, yaml).await?;

    // Output JSON result
    let output = InitOutput {
        created: Utc::now().to_rfc3339(),
        file: args.output.display().to_string(),
    };
    println!("{}", serde_json::to_string(&output)?);

    Ok(())
}
