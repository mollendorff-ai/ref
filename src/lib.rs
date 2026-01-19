//! Möllendorff Ref: LLM-optimized reference toolkit
//!
//! JSON output for agents, not humans.
//! Bypasses bot protection via headless Chrome.

pub mod browser;
pub mod check_links;
pub mod extract;
pub mod fetch;
pub mod init;
pub mod pdf;
pub mod refresh_data;
pub mod scan;
pub mod schema;
pub mod update;
pub mod verify_refs;

pub use check_links::{check_links, CheckLinksConfig, LinkReport, LinkResult};
pub use refresh_data::{refresh_data, ExtractedData, RefreshConfig};
pub use schema::{Meta, Reference, ReferencesFile, Status};
