//! [`ModelCatalog`](mirri_core::ModelCatalog) implementation backed by the
//! embedded server's `ServerConfig` snapshot.
//!
//! The config lives behind an `Arc<RwLock<ServerConfig>>` so the catalog is
//! always built from the same source of truth the `config.get` / `model.list`
//! RPCs read — including config hot-reloads, which swap the snapshot in
//! place.

use std::sync::{Arc, RwLock};

use mirri_core::{ModelCatalog, ModelCatalogEntry, ServerConfig};

/// A [`ModelCatalog`] that reads model aliases from a [`ServerConfig`].
pub struct ConfigModelCatalog {
    config: Arc<RwLock<ServerConfig>>,
}

impl ConfigModelCatalog {
    /// Wrap a shared config snapshot.
    pub fn new(config: Arc<RwLock<ServerConfig>>) -> Self {
        Self { config }
    }
}

impl ModelCatalog for ConfigModelCatalog {
    fn list_models(&self) -> Vec<ModelCatalogEntry> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        model_entries_from_config(&config)
    }

    fn get_by_id(&self, id: &str) -> Option<ModelCatalogEntry> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let entries = model_entries_from_config(&config);
        if let Some(entry) = entries.iter().find(|entry| entry.id == id) {
            return Some(entry.clone());
        }
        // The `${provider}/${model}` composite convention (the C6 id the web
        // pickers send) resolves to the configured alias naming the same
        // provider + model pair — the TS registry serves such composites as
        // generated alias keys via the models.dev import, so a config
        // keeping the pair under another alias key still resolves.
        let key = config.resolve_model_alias(id)?;
        entries.into_iter().find(|entry| entry.id == key)
    }
}

/// Build catalog entries from a config snapshot.
///
/// Delegates to the single core implementation
/// ([`mirri_core::model_entries_from_config`]) — the same resolution the
/// serve wire's `session.status` context-window read uses.
pub(crate) fn model_entries_from_config(config: &ServerConfig) -> Vec<ModelCatalogEntry> {
    mirri_core::model_entries_from_config(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_core::ModelAlias;

    fn config_with(models: Vec<(String, ModelAlias)>) -> Arc<RwLock<ServerConfig>> {
        let mut config = ServerConfig::default();
        if !models.is_empty() {
            config.models = Some(models.into_iter().collect());
        }
        Arc::new(RwLock::new(config))
    }

    #[test]
    fn should_list_alias_entries_when_config_has_models_map() {
        // Given a config with two model aliases
        let config = config_with(vec![
            (
                "sonnet".into(),
                ModelAlias {
                    provider: "anthropic".into(),
                    model: "claude-sonnet-4-20250514".into(),
                    max_context_size: 200_000,
                    description: Some("The balanced model".into()),
                    ..ModelAlias::default()
                },
            ),
            (
                "gpt4o".into(),
                ModelAlias {
                    provider: "openai".into(),
                    model: "gpt-4o".into(),
                    ..ModelAlias::default()
                },
            ),
        ]);
        let catalog = ConfigModelCatalog::new(config);

        // When listing models
        let entries = catalog.list_models();

        // Then each alias is an entry keyed by its id
        assert_eq!(entries.len(), 2);
        let sonnet = catalog.get_by_id("sonnet").expect("sonnet present");
        assert_eq!(sonnet.model, "claude-sonnet-4-20250514");
        assert_eq!(sonnet.provider, "anthropic");
        assert_eq!(sonnet.max_context_size, Some(200_000));
        assert_eq!(sonnet.description.as_deref(), Some("The balanced model"));
        assert!(catalog.get_by_id("nope").is_none());
    }

    #[test]
    fn should_fall_back_to_legacy_model_section_when_no_models_map() {
        // Given a config with only legacy `[model]`
        let mut config = ServerConfig::default();
        config.model = Some(mirri_core::config::ModelConfig {
            id: "claude-sonnet-4-20250514".into(),
            provider: Some("anthropic".into()),
        });
        let catalog = ConfigModelCatalog::new(Arc::new(RwLock::new(config)));

        // When listing models
        let entries = catalog.list_models();

        // Then a single entry exists keyed by the model id
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "claude-sonnet-4-20250514");
        assert_eq!(entries[0].provider, "anthropic");
        assert!(catalog.get_by_id("claude-sonnet-4-20250514").is_some());
    }
}
