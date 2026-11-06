//! Möllendorff Ref: LLM-optimized reference toolkit
//!
//! JSON output for agents, not humans.
//! Renders pages in headless Chrome, including JavaScript-heavy sites.

pub mod access;
pub mod browser;
pub mod check_links;
pub mod content;
pub mod cookies;
pub mod extract;
pub mod fetch;
pub mod guard;
pub mod init;
pub mod mcp;
pub mod pdf;
pub mod refresh_data;
pub mod scan;
pub mod schema;
pub mod text;
pub mod update;
pub mod verify_refs;

pub use check_links::{check_links, CheckLinksConfig, LinkReport, LinkResult};
pub use refresh_data::{refresh_data, ExtractedData, RefreshConfig};
pub use schema::{Meta, Reference, ReferencesFile, Status};
