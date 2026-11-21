//! Möllendorff Ref: renders web pages and PDFs into compact JSON for LLM agents.
//!
//! JSON output for agents, not humans.
//! Pages are rendered in headless Chrome, including JavaScript-heavy sites.

pub mod access;
pub mod browser;
pub mod check_links;
pub mod cli;
pub mod content;
pub mod cookies;
pub mod extract;
pub mod fetch;
pub mod guard;
pub mod init;
pub mod login;
pub mod mcp;
pub mod pdf;
pub mod profile;
pub mod refresh_data;
pub mod scan;
pub mod schema;
pub mod task;
#[cfg(test)]
pub(crate) mod test_server;
pub mod text;
pub mod update;
pub mod verify_refs;

pub use check_links::{check_links, CheckLinksConfig, LinkReport, LinkResult};
pub use refresh_data::{refresh_data, ExtractedData, RefreshConfig};
pub use schema::{Meta, Reference, ReferencesFile, Status};
