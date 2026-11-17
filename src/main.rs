//! Möllendorff Ref CLI
//!
//! Renders web pages and PDFs into compact JSON for LLM agents.
//! Pages are rendered in headless Chrome, including JavaScript-heavy sites.
//! Everything is in the library (`mollendorff_ref::cli`), where it is tested.

fn main() -> std::process::ExitCode {
    mollendorff_ref::cli::main()
}
