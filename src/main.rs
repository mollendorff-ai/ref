//! Möllendorff Ref CLI
//!
//! LLM-optimized reference tools with headless Chrome.
//! Renders pages in headless Chrome, including JavaScript-heavy sites.
//! Everything is in the library (`mollendorff_ref::cli`), where it is tested.

fn main() -> std::process::ExitCode {
    mollendorff_ref::cli::main()
}
