//! Provider credential store — the single durable home for provider API
//! keys (docs/specs/provider-credential-*.md). One credential per
//! provider (SDR-004), plaintext with per-entry encoding declaration
//! (ADR-008), atomic writes under a cross-process lock.

pub mod store;
pub use store::{CredentialRecord, CredentialStore, CredentialValue, Encoding};

pub mod external;
pub use external::{ExternalSourceError, external_available, obtain_external};

pub mod migration;
pub use migration::{MigrationReport, SKIPPED_GUIDANCE, dry_run_config_api_keys, migrate_config_api_keys};

#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod migration_tests;
#[cfg(test)]
mod external_tests;
