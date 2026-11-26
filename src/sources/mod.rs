pub mod netease;
pub mod quickjs_plugin;

use std::path::PathBuf;
use std::sync::Arc;

pub use netease::NetEaseSource;
pub use quickjs_plugin::{QuickJsSourcePlugin, discover_quickjs_plugins};

use crate::core::traits::SourceRegistry;

/// Builds the default `SourceRegistry` with the built-in NetEase source followed by
/// QuickJS plugins discovered from `plugin_dirs`.
pub fn build_default_source_registry(plugin_dirs: &[PathBuf], artist_sep: &str) -> SourceRegistry {
    let mut registry = SourceRegistry::new();
    registry.register(Arc::new(NetEaseSource::new(artist_sep)));

    for plugin in discover_quickjs_plugins(plugin_dirs) {
        registry.register(Arc::new(plugin));
    }

    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_includes_netease_and_example_template_plugin() {
        let plugin_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("plugins")
            .join("sources");
        let registry = build_default_source_registry(&[plugin_dir], ";");

        let list = registry.list();
        assert!(list.iter().any(|s| s.id == "netease" && s.builtin && s.enabled));
        assert!(
            list.iter()
                .any(|s| s.id == "example-custom-js" && !s.builtin && !s.enabled)
        );

        let active = registry.active_combo().expect("netease should be active");
        assert_eq!(active.info().id, "netease");
        assert!(active.info().capabilities.key163);

        let example = registry
            .get("example-custom-js")
            .expect("example-custom-js should be registered");
        let empty_res = example
            .search_combo(&crate::core::models::SearchQuery::default())
            .expect("empty query returns empty list");
        assert!(empty_res.is_empty());
        let lrc = example
            .fetch_lyrics("123")
            .expect("example fetch_lyrics executes cleanly");
        assert!(lrc.uncollected);
    }
}
