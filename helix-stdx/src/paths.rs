//! Product-specific filesystem locations.

use std::path::PathBuf;

use etcetera::base_strategy::{choose_base_strategy, BaseStrategy};

pub const PRODUCT_CONFIG_DIR: &str = "double-helix";
pub const LEGACY_CONFIG_DIR: &str = "helix";

/// Returns the directory containing user-editable Double Helix configuration.
#[must_use]
pub fn config_dir() -> PathBuf {
    base_strategy().config_dir().join(PRODUCT_CONFIG_DIR)
}

/// Returns the legacy Helix configuration directory used by migration code.
#[must_use]
pub fn legacy_config_dir() -> PathBuf {
    base_strategy().config_dir().join(LEGACY_CONFIG_DIR)
}

/// Returns the directory containing rebuildable Double Helix cache data.
#[must_use]
pub fn cache_dir() -> PathBuf {
    base_strategy().cache_dir().join(PRODUCT_CONFIG_DIR)
}

/// Returns the directory containing durable Double Helix application data.
#[must_use]
pub fn data_dir() -> PathBuf {
    base_strategy().data_dir().join(PRODUCT_CONFIG_DIR)
}

fn base_strategy() -> impl BaseStrategy {
    choose_base_strategy().expect("Unable to find the platform data directories!")
}

#[cfg(test)]
mod tests {
    use super::{cache_dir, config_dir, data_dir, legacy_config_dir};

    #[test]
    fn product_directories_use_stable_leaf_names() {
        assert_eq!(config_dir().file_name().unwrap(), "double-helix");
        assert_eq!(cache_dir().file_name().unwrap(), "double-helix");
        assert_eq!(data_dir().file_name().unwrap(), "double-helix");
        assert_eq!(legacy_config_dir().file_name().unwrap(), "helix");
    }
}
