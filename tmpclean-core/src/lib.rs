//! Deletion engine for tmpclean.

#[cfg(windows)]
mod win;

/// Crate version, exposed for the binaries.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
