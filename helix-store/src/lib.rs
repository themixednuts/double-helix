mod backend;
mod dto;
mod error;
mod migrations;
mod repos;
mod schema;

use std::path::PathBuf;
use std::time::Duration;

pub use backend::{Backend, SqliteValue};
pub use dto::{
    AssistantLayout, AssistantPermission, AssistantThread, FrecencyEntry, PkgReceipt, QueryHistory,
};
pub use error::{Error, Result};
pub use repos::{
    AssistantLayoutRepo, AssistantPermissionsRepo, AssistantThreadsRepo, FrecencyRepo,
    PkgReceiptsRepo, QueryHistoryRepo,
};

use backend::DrizzleBackend;

const PRODUCT_CONFIG_DIR: &str = "double-helix";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePaths {
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl StorePaths {
    #[must_use]
    pub fn default_paths() -> Self {
        Self {
            state: data_dir().join("state.sqlite3"),
            cache: helix_loader::cache_dir().join("cache.sqlite3"),
        }
    }

    #[must_use]
    pub fn new(state: impl Into<PathBuf>, cache: impl Into<PathBuf>) -> Self {
        Self {
            state: state.into(),
            cache: cache.into(),
        }
    }
}

pub struct Store {
    state: DrizzleBackend,
    cache: DrizzleBackend,
}

pub struct CacheStore {
    cache: DrizzleBackend,
}

impl Store {
    /// Opens the default durable and rebuildable-cache databases.
    ///
    /// # Errors
    ///
    /// Returns an error if a database path cannot be prepared, opened, configured, or migrated.
    pub fn open_default() -> Result<Self> {
        Self::open(StorePaths::default_paths())
    }

    /// Opens the durable and rebuildable-cache databases at explicit paths.
    ///
    /// # Errors
    ///
    /// Returns an error if a database path cannot be prepared, opened, configured, or migrated.
    pub fn open(paths: StorePaths) -> Result<Self> {
        let state = DrizzleBackend::open(paths.state, DatabaseKind::State)?;
        let cache = DrizzleBackend::open(paths.cache, DatabaseKind::Cache)?;
        Ok(Self { state, cache })
    }

    #[must_use]
    pub fn threads(&mut self) -> AssistantThreadsRepo<'_> {
        AssistantThreadsRepo::new(&mut self.state)
    }

    #[must_use]
    pub fn layout(&mut self) -> AssistantLayoutRepo<'_> {
        AssistantLayoutRepo::new(&mut self.state)
    }

    #[must_use]
    pub fn permissions(&mut self) -> AssistantPermissionsRepo<'_> {
        AssistantPermissionsRepo::new(&mut self.state)
    }

    #[must_use]
    pub fn frecency(&mut self) -> FrecencyRepo<'_> {
        FrecencyRepo::new(&mut self.cache)
    }

    #[must_use]
    pub fn query_history(&mut self) -> QueryHistoryRepo<'_> {
        QueryHistoryRepo::new(&mut self.cache)
    }

    #[must_use]
    pub fn receipts(&mut self) -> PkgReceiptsRepo<'_> {
        PkgReceiptsRepo::new(&mut self.state)
    }

    /// Returns the SQLite journal mode currently reported by the durable state database.
    ///
    /// # Errors
    ///
    /// Returns an error if the pragma query fails.
    pub fn state_journal_mode(&mut self) -> Result<String> {
        self.state.journal_mode()
    }

    /// Returns the SQLite journal mode currently reported by the cache database.
    ///
    /// # Errors
    ///
    /// Returns an error if the pragma query fails.
    pub fn cache_journal_mode(&mut self) -> Result<String> {
        self.cache.journal_mode()
    }
}

impl CacheStore {
    /// Opens only the default rebuildable-cache database.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache database path cannot be prepared, opened, configured, or migrated.
    pub fn open_default() -> Result<Self> {
        Self::open(StorePaths::default_paths())
    }

    /// Opens only the rebuildable-cache database from explicit paths.
    ///
    /// The durable state path is ignored; callers that need assistant or package state should use
    /// [`Store`] instead.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache database path cannot be prepared, opened, configured, or migrated.
    pub fn open(paths: StorePaths) -> Result<Self> {
        let cache = DrizzleBackend::open(paths.cache, DatabaseKind::Cache)?;
        Ok(Self { cache })
    }

    #[must_use]
    pub fn frecency(&mut self) -> FrecencyRepo<'_> {
        FrecencyRepo::new(&mut self.cache)
    }

    #[must_use]
    pub fn query_history(&mut self) -> QueryHistoryRepo<'_> {
        QueryHistoryRepo::new(&mut self.cache)
    }

    /// Returns the SQLite journal mode currently reported by the cache database.
    ///
    /// # Errors
    ///
    /// Returns an error if the pragma query fails.
    pub fn cache_journal_mode(&mut self) -> Result<String> {
        self.cache.journal_mode()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DatabaseKind {
    State,
    Cache,
}

fn data_dir() -> PathBuf {
    use etcetera::base_strategy::{choose_base_strategy, BaseStrategy};

    choose_base_strategy()
        .expect("Unable to find the data directory!")
        .data_dir()
        .join(PRODUCT_CONFIG_DIR)
}

pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
