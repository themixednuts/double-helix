use crate::db_healthcheck::DbHealthChecker;
use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_HISTORY_ENTRIES: usize = 128;

pub type QueryPersistenceError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type QueryPersistenceResult<T> = std::result::Result<T, QueryPersistenceError>;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct QueryMatchEntry {
    pub file_path: PathBuf,
    pub open_count: u32,
    pub last_opened: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryHistoryKind {
    File,
    Grep,
}

pub trait QueryTrackerStore: std::fmt::Debug + Send + Sync {
    fn load_match(
        &self,
        project_path: &Path,
        query: &str,
    ) -> QueryPersistenceResult<Option<QueryMatchEntry>>;
    fn save_match(
        &self,
        project_path: &Path,
        query: &str,
        entry: &QueryMatchEntry,
    ) -> QueryPersistenceResult<()>;
    fn append_history(
        &self,
        project_path: &Path,
        kind: QueryHistoryKind,
        query: &str,
        timestamp: u64,
    ) -> QueryPersistenceResult<()>;
    fn history_at(
        &self,
        project_path: &Path,
        kind: QueryHistoryKind,
        offset: usize,
    ) -> QueryPersistenceResult<Option<String>>;
    fn entry_counts(&self) -> QueryPersistenceResult<Vec<(&'static str, u64)>>;
    fn disk_size(&self) -> QueryPersistenceResult<u64> {
        Ok(0)
    }
    fn location(&self) -> String {
        "memory".to_string()
    }
}

#[derive(Debug, Default)]
pub struct InMemoryQueryTrackerStore {
    matches: Mutex<HashMap<(PathBuf, String), QueryMatchEntry>>,
    histories: Mutex<HashMap<(PathBuf, QueryHistoryKind), VecDeque<String>>>,
}

impl InMemoryQueryTrackerStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl QueryTrackerStore for InMemoryQueryTrackerStore {
    fn load_match(
        &self,
        project_path: &Path,
        query: &str,
    ) -> QueryPersistenceResult<Option<QueryMatchEntry>> {
        Ok(self
            .matches
            .lock()
            .map_err(|_| "query tracker store lock poisoned")?
            .get(&(project_path.to_path_buf(), query.to_string()))
            .cloned())
    }

    fn save_match(
        &self,
        project_path: &Path,
        query: &str,
        entry: &QueryMatchEntry,
    ) -> QueryPersistenceResult<()> {
        self.matches
            .lock()
            .map_err(|_| "query tracker store lock poisoned")?
            .insert(
                (project_path.to_path_buf(), query.to_string()),
                entry.clone(),
            );
        Ok(())
    }

    fn append_history(
        &self,
        project_path: &Path,
        kind: QueryHistoryKind,
        query: &str,
        _timestamp: u64,
    ) -> QueryPersistenceResult<()> {
        let mut histories = self
            .histories
            .lock()
            .map_err(|_| "query tracker store lock poisoned")?;
        let history = histories
            .entry((project_path.to_path_buf(), kind))
            .or_default();
        history.push_back(query.to_string());
        while history.len() > MAX_HISTORY_ENTRIES {
            history.pop_front();
        }
        Ok(())
    }

    fn history_at(
        &self,
        project_path: &Path,
        kind: QueryHistoryKind,
        offset: usize,
    ) -> QueryPersistenceResult<Option<String>> {
        let histories = self
            .histories
            .lock()
            .map_err(|_| "query tracker store lock poisoned")?;
        let query = histories
            .get(&(project_path.to_path_buf(), kind))
            .and_then(|history| {
                history
                    .len()
                    .checked_sub(1 + offset)
                    .and_then(|idx| history.get(idx))
            })
            .cloned();
        Ok(query)
    }

    fn entry_counts(&self) -> QueryPersistenceResult<Vec<(&'static str, u64)>> {
        let matches = self
            .matches
            .lock()
            .map_err(|_| "query tracker store lock poisoned")?
            .len() as u64;
        let histories = self
            .histories
            .lock()
            .map_err(|_| "query tracker store lock poisoned")?;
        let file_history = histories
            .iter()
            .filter(|((_, kind), _)| *kind == QueryHistoryKind::File)
            .map(|(_, history)| history.len() as u64)
            .sum();
        let grep_history = histories
            .iter()
            .filter(|((_, kind), _)| *kind == QueryHistoryKind::Grep)
            .map(|(_, history)| history.len() as u64)
            .sum();

        Ok(vec![
            ("query_file_entries", matches),
            ("query_history_entries", file_history),
            ("grep_query_history_entries", grep_history),
        ])
    }
}

#[derive(Debug)]
pub struct QueryTracker {
    store: Box<dyn QueryTrackerStore>,
}

impl DbHealthChecker for QueryTracker {
    fn get_health(&self) -> Result<crate::db_healthcheck::DbHealth, Error> {
        Ok(crate::db_healthcheck::DbHealth {
            path: self.store.location(),
            disk_size: self.store.disk_size().map_err(storage_error)?,
            entry_counts: self.store.entry_counts().map_err(storage_error)?,
        })
    }
}

impl QueryTracker {
    pub fn new(store: impl QueryTrackerStore + 'static) -> Self {
        Self {
            store: Box::new(store),
        }
    }

    pub fn memory_only() -> Self {
        Self::new(InMemoryQueryTrackerStore::new())
    }

    fn get_now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[cfg(test)]
    fn create_query_key(project_path: &Path, query: &str) -> Result<[u8; 32], Error> {
        let project_str = project_path
            .to_str()
            .ok_or_else(|| Error::InvalidPath(project_path.to_path_buf()))?;

        let mut hasher = blake3::Hasher::default();
        hasher.update(project_str.as_bytes());
        hasher.update(b"::");
        hasher.update(query.as_bytes());

        Ok(*hasher.finalize().as_bytes())
    }

    #[cfg(test)]
    fn create_project_key(project_path: &Path) -> Result<[u8; 32], Error> {
        let project_str = project_path
            .to_str()
            .ok_or_else(|| Error::InvalidPath(project_path.to_path_buf()))?;

        Ok(*blake3::hash(project_str.as_bytes()).as_bytes())
    }

    pub fn track_query_completion(
        &mut self,
        query: &str,
        project_path: &Path,
        file_path: &Path,
    ) -> Result<(), Error> {
        let now = self.get_now();
        let file_path_buf = file_path.to_path_buf();

        let mut entry = self
            .store
            .load_match(project_path, query)
            .map_err(storage_error)?
            .unwrap_or_else(|| QueryMatchEntry {
                file_path: file_path_buf.clone(),
                open_count: 0,
                last_opened: now,
            });

        if entry.file_path == file_path_buf {
            tracing::debug!(
                ?query,
                ?file_path,
                "Query completed for same file as last time"
            );
            entry.open_count += 1;
        } else {
            tracing::debug!(
                ?query,
                ?file_path,
                "Query completed for different file than last time"
            );
            entry.file_path = file_path_buf;
            entry.open_count = 1;
        }

        entry.last_opened = now;
        self.store
            .save_match(project_path, query, &entry)
            .map_err(storage_error)?;
        self.store
            .append_history(project_path, QueryHistoryKind::File, query, now)
            .map_err(storage_error)?;

        tracing::debug!(?query, ?file_path, "Tracked query completion");
        Ok(())
    }

    pub fn get_last_query_entry(
        &self,
        query: &str,
        project_path: &Path,
        min_combo_count: u32,
    ) -> Result<Option<QueryMatchEntry>, Error> {
        let last_match = self
            .store
            .load_match(project_path, query)
            .map_err(storage_error)?;

        Ok(last_match.filter(|entry| entry.open_count >= min_combo_count))
    }

    pub fn get_last_query_path(
        &self,
        query: &str,
        project_path: &Path,
        file_path: &Path,
        combo_boost: i32,
    ) -> Result<i32, Error> {
        match self
            .store
            .load_match(project_path, query)
            .map_err(storage_error)?
        {
            Some(entry) if entry.file_path == file_path && entry.open_count >= 2 => Ok(combo_boost),
            _ => Ok(0),
        }
    }

    pub fn get_historical_query(
        &self,
        project_path: &Path,
        offset: usize,
    ) -> Result<Option<String>, Error> {
        self.store
            .history_at(project_path, QueryHistoryKind::File, offset)
            .map_err(storage_error)
    }

    pub fn track_grep_query(&mut self, query: &str, project_path: &Path) -> Result<(), Error> {
        let now = self.get_now();
        self.store
            .append_history(project_path, QueryHistoryKind::Grep, query, now)
            .map_err(storage_error)?;

        tracing::debug!(?query, "Tracked grep query");
        Ok(())
    }

    pub fn get_historical_grep_query(
        &self,
        project_path: &Path,
        offset: usize,
    ) -> Result<Option<String>, Error> {
        self.store
            .history_at(project_path, QueryHistoryKind::Grep, offset)
            .map_err(storage_error)
    }
}

fn storage_error(error: QueryPersistenceError) -> Error {
    Error::Persistence(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_query_tracking() {
        let mut tracker = QueryTracker::memory_only();

        let project_path = PathBuf::from("/test/project");
        let file_path = PathBuf::from("/test/project/src/main.rs");

        tracker
            .track_query_completion("main", &project_path, &file_path)
            .unwrap();
        let boost = tracker
            .get_last_query_path("main", &project_path, &file_path, 10000)
            .unwrap();
        assert_eq!(boost, 0, "First completion should not boost");

        tracker
            .track_query_completion("main", &project_path, &file_path)
            .unwrap();
        let boost = tracker
            .get_last_query_path("main", &project_path, &file_path, 10000)
            .unwrap();
        assert_eq!(boost, 10000, "Second completion should boost");

        let other_file = PathBuf::from("/test/project/src/lib.rs");
        tracker
            .track_query_completion("main", &project_path, &other_file)
            .unwrap();
        let boost = tracker
            .get_last_query_path("main", &project_path, &other_file, 10000)
            .unwrap();
        assert_eq!(boost, 0, "Different file should reset boost");

        let boost = tracker
            .get_last_query_path("main", &project_path, &file_path, 10000)
            .unwrap();
        assert_eq!(boost, 0, "Original file should not boost after replacement");
    }

    #[test]
    fn test_query_histories_are_bounded_and_separate() {
        let mut tracker = QueryTracker::memory_only();
        let project_path = PathBuf::from("/test/project");
        let file_path = PathBuf::from("/test/project/src/main.rs");

        for index in 0..140 {
            tracker
                .track_query_completion(&format!("file-{index}"), &project_path, &file_path)
                .unwrap();
            tracker
                .track_grep_query(&format!("grep-{index}"), &project_path)
                .unwrap();
        }

        assert_eq!(
            tracker.get_historical_query(&project_path, 0).unwrap(),
            Some("file-139".to_string())
        );
        assert_eq!(
            tracker.get_historical_query(&project_path, 127).unwrap(),
            Some("file-12".to_string())
        );
        assert_eq!(
            tracker.get_historical_query(&project_path, 128).unwrap(),
            None
        );
        assert_eq!(
            tracker.get_historical_grep_query(&project_path, 0).unwrap(),
            Some("grep-139".to_string())
        );
    }

    #[test]
    fn test_hashing_functions() {
        let project_path = PathBuf::from("/test/project");

        let key1 = QueryTracker::create_project_key(&project_path).unwrap();
        let key2 = QueryTracker::create_project_key(&project_path).unwrap();
        assert_eq!(key1, key2, "Same project should hash to same key");

        let query_key1 = QueryTracker::create_query_key(&project_path, "test").unwrap();
        let query_key2 = QueryTracker::create_query_key(&project_path, "test").unwrap();
        assert_eq!(
            query_key1, query_key2,
            "Same project+query should hash to same key"
        );

        let query_key3 = QueryTracker::create_query_key(&project_path, "different").unwrap();
        assert_ne!(
            query_key1, query_key3,
            "Different queries should hash differently"
        );

        let other_project = PathBuf::from("/other/project");
        let query_key4 = QueryTracker::create_query_key(&other_project, "test").unwrap();
        assert_ne!(
            query_key1, query_key4,
            "Different projects should hash differently"
        );
    }

    #[test]
    fn test_env_temp_dir_does_not_affect_memory_store() {
        let _ = env::temp_dir();
        let tracker = QueryTracker::memory_only();
        assert_eq!(
            tracker
                .store
                .entry_counts()
                .unwrap()
                .into_iter()
                .map(|(_, count)| count)
                .sum::<u64>(),
            0
        );
    }
}
