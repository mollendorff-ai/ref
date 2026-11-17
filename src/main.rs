//! Möllendorff Ref CLI
//!
//! LLM-optimized reference tools with headless Chrome.
//! Renders pages in headless Chrome, including JavaScript-heavy sites.

use std::process::ExitCode;

fn main() -> ExitCode {
    mollendorff_ref::cli::main()
}

#[cfg(test)]
mod tests {
    #[test]
    fn main_runs_the_command_line() {
        // The test program's own arguments are not a ref command line, so ref
        // refuses them with exit code 2 (it never starts Chrome)
        let code = super::main();
        assert_eq!(
            format!("{code:?}"),
            format!("{:?}", std::process::ExitCode::from(2))
        );
    }
}
