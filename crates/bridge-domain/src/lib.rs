//! Minimal domain crate for the agent-bridge Rust implementation.
//!
//! Intentionally contains no business types yet; it exists so that the
//! workspace wiring can be built and tested end to end.

/// Returns the package name as a trivial smoke-check helper.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::crate_name;

    #[test]
    fn crate_is_wired() {
        assert_eq!(crate_name(), "bridge-domain");
    }
}
