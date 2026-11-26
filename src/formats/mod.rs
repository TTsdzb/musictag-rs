pub mod lofty_handler;

use std::sync::Arc;

use crate::core::traits::FormatRegistry;

pub use lofty_handler::{LoftyFormatHandler, extract_cover_to_sidecar, process_cover_image};

/// Builds the default `FormatRegistry` backed by `LoftyFormatHandler`.
pub fn build_default_format_registry() -> FormatRegistry {
    let mut registry = FormatRegistry::new();
    registry.register(Arc::new(LoftyFormatHandler::new()));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_registers_all_supported_extensions() {
        let reg = build_default_format_registry();
        for ext in [
            "flac", "mp3", "m4a", "mp4", "ogg", "opus", "wav", "aiff", "aif", "aifc", "ape", "wv",
            "mpc", "aac",
        ] {
            assert!(
                reg.by_extension(ext).is_some(),
                "missing handler for .{ext}"
            );
        }
    }
}
