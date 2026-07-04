// FFF_STORAGE_TRAITS_BLOCKER: Helix keeps fff-search storage-agnostic.
use super::db_healthcheck::DbHealthChecker;
use crate::error::{Error, Result};
use crate::file_picker::FFFMode;
use crate::git::is_modified_status;
use crate::shared::SharedFrecency;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

const DECAY_CONSTANT: f64 = 0.0693; // ln(2)/10 for 10-day half-life
const SECONDS_PER_DAY: f64 = 86400.0;
const MAX_HISTORY_DAYS: f64 = 30.0;
const MAX_TIMESTAMPS_PER_FILE: usize = 128;

const AI_DECAY_CONSTANT: f64 = 0.231; // ln(2)/3 for 3-day half-life
const AI_MAX_HISTORY_DAYS: f64 = 7.0;

pub type FrecencyPersistenceError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type FrecencyPersistenceResult<T> = std::result::Result<T, FrecencyPersistenceError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrecencyRecord {
    pub path_hash: [u8; 32],
    pub accesses: VecDeque<u64>,
}

pub trait FrecencyStore: std::fmt::Debug + Send + Sync {
    fn load_all(&self) -> FrecencyPersistenceResult<Vec<FrecencyRecord>>;
    fn save(&self, path_hash: &[u8; 32], accesses: &VecDeque<u64>)
    -> FrecencyPersistenceResult<()>;
    fn delete(&self, path_hash: &[u8; 32]) -> FrecencyPersistenceResult<()>;
    fn entry_count(&self) -> FrecencyPersistenceResult<u64>;
    fn disk_size(&self) -> FrecencyPersistenceResult<u64> {
        Ok(0)
    }
    fn location(&self) -> String {
        "memory".to_string()
    }
}

#[derive(Debug, Default)]
pub struct InMemoryFrecencyStore {
    entries: Mutex<HashMap<[u8; 32], VecDeque<u64>>>,
}

impl InMemoryFrecencyStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl FrecencyStore for InMemoryFrecencyStore {
    fn load_all(&self) -> FrecencyPersistenceResult<Vec<FrecencyRecord>> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| "frecency store lock poisoned")?;
        Ok(entries
            .iter()
            .map(|(path_hash, accesses)| FrecencyRecord {
                path_hash: *path_hash,
                accesses: accesses.clone(),
            })
            .collect())
    }

    fn save(
        &self,
        path_hash: &[u8; 32],
        accesses: &VecDeque<u64>,
    ) -> FrecencyPersistenceResult<()> {
        self.entries
            .lock()
            .map_err(|_| "frecency store lock poisoned")?
            .insert(*path_hash, accesses.clone());
        Ok(())
    }

    fn delete(&self, path_hash: &[u8; 32]) -> FrecencyPersistenceResult<()> {
        self.entries
            .lock()
            .map_err(|_| "frecency store lock poisoned")?
            .remove(path_hash);
        Ok(())
    }

    fn entry_count(&self) -> FrecencyPersistenceResult<u64> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| "frecency store lock poisoned")?
            .len() as u64)
    }
}

#[derive(Debug)]
pub struct FrecencyTracker {
    store: Box<dyn FrecencyStore>,
    accesses: RwLock<HashMap<[u8; 32], VecDeque<u64>>>,
}

const MODIFICATION_THRESHOLDS: [(i64, u64); 5] = [
    (16, 60 * 2),
    (8, 60 * 15),
    (4, 60 * 60),
    (2, 60 * 60 * 24),
    (1, 60 * 60 * 24 * 7),
];

const AI_MODIFICATION_THRESHOLDS: [(i64, u64); 5] = [
    (16, 30),
    (8, 60 * 5),
    (4, 60 * 15),
    (2, 60 * 60),
    (1, 60 * 60 * 4),
];

impl DbHealthChecker for FrecencyTracker {
    fn get_health(&self) -> Result<super::db_healthcheck::DbHealth> {
        Ok(super::db_healthcheck::DbHealth {
            path: self.store.location(),
            disk_size: self.store.disk_size().map_err(storage_error)?,
            entry_counts: vec![(
                "absolute_frecency_entries",
                self.store.entry_count().map_err(storage_error)?,
            )],
        })
    }
}

impl FrecencyTracker {
    pub fn new(store: impl FrecencyStore + 'static) -> Result<Self> {
        let records = store.load_all().map_err(storage_error)?;
        let accesses = records
            .into_iter()
            .map(|record| (record.path_hash, record.accesses))
            .collect();
        Ok(Self {
            store: Box::new(store),
            accesses: RwLock::new(accesses),
        })
    }

    pub fn memory_only() -> Result<Self> {
        Self::new(InMemoryFrecencyStore::new())
    }

    pub fn spawn_gc(
        shared: SharedFrecency,
        _db_path: String,
        _use_unsafe_no_lock: bool,
    ) -> Result<std::thread::JoinHandle<()>> {
        Ok(std::thread::Builder::new()
            .name("fff-frecency-gc".into())
            .spawn(move || Self::run_frecency_gc(shared))?)
    }

    #[tracing::instrument(skip(shared))]
    fn run_frecency_gc(shared: SharedFrecency) {
        let start = std::time::Instant::now();
        let (deleted, pruned) = {
            let guard = match shared.read() {
                Ok(g) => g,
                Err(e) => {
                    tracing::debug!("Failed to acquire read lock: {e}");
                    return;
                }
            };
            let Some(ref tracker) = *guard else {
                return;
            };
            match tracker.purge_stale_entries() {
                Ok(result) => result,
                Err(e) => {
                    tracing::debug!("Purge failed: {e}");
                    return;
                }
            }
        };

        if deleted > 0 || pruned > 0 {
            tracing::info!(deleted, pruned, elapsed = ?start.elapsed(), "Frecency GC purged entries");
        }
    }

    fn purge_stale_entries(&self) -> Result<(usize, usize)> {
        let now = self.get_now();
        let cutoff_time = now.saturating_sub((MAX_HISTORY_DAYS * SECONDS_PER_DAY) as u64);
        let mut deleted = Vec::new();
        let mut updated = Vec::new();

        {
            let mut entries = self
                .accesses
                .write()
                .map_err(|_| Error::AcquireFrecencyLock)?;
            entries.retain(|path_hash, accesses| {
                let fresh_start = accesses.iter().position(|&ts| ts >= cutoff_time);
                match fresh_start {
                    None => {
                        deleted.push(*path_hash);
                        false
                    }
                    Some(0) => true,
                    Some(start) => {
                        let pruned = accesses.iter().skip(start).copied().collect();
                        *accesses = pruned;
                        updated.push((*path_hash, accesses.clone()));
                        true
                    }
                }
            });
        }

        for path_hash in &deleted {
            self.store.delete(path_hash).map_err(storage_error)?;
        }
        for (path_hash, accesses) in &updated {
            self.store
                .save(path_hash, accesses)
                .map_err(storage_error)?;
        }

        Ok((deleted.len(), updated.len()))
    }

    fn get_accesses(&self, path: &Path) -> Result<Option<VecDeque<u64>>> {
        let key_hash = Self::path_to_hash_bytes(path)?;
        let entries = self
            .accesses
            .read()
            .map_err(|_| Error::AcquireFrecencyLock)?;
        Ok(entries.get(&key_hash).cloned())
    }

    fn get_now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn path_to_hash_bytes(path: &Path) -> Result<[u8; 32]> {
        let Some(key) = path.to_str() else {
            return Err(Error::InvalidPath(path.to_path_buf()));
        };

        Ok(*blake3::hash(key.as_bytes()).as_bytes())
    }

    pub fn seconds_since_last_access(&self, path: &Path) -> Result<Option<u64>> {
        let accesses = self.get_accesses(path)?;
        let last = accesses.and_then(|a| a.back().copied());
        Ok(last.map(|ts| self.get_now().saturating_sub(ts)))
    }

    pub fn access_count(&self, path: &Path) -> Result<usize> {
        Ok(self.get_accesses(path)?.map_or(0, |a| a.len()))
    }

    pub fn track_access(&self, path: &Path) -> Result<()> {
        let key_hash = Self::path_to_hash_bytes(path)?;
        let now = self.get_now();
        let cutoff_time = now.saturating_sub((MAX_HISTORY_DAYS * SECONDS_PER_DAY) as u64);
        let updated = {
            let mut entries = self
                .accesses
                .write()
                .map_err(|_| Error::AcquireFrecencyLock)?;
            let accesses = entries.entry(key_hash).or_default();
            while let Some(&front_time) = accesses.front() {
                if front_time < cutoff_time || accesses.len() >= MAX_TIMESTAMPS_PER_FILE {
                    accesses.pop_front();
                } else {
                    break;
                }
            }
            accesses.push_back(now);
            accesses.clone()
        };

        tracing::debug!(?path, accesses = updated.len(), "Tracking access");
        self.store.save(&key_hash, &updated).map_err(storage_error)
    }

    pub fn get_access_score(&self, file_path: &Path, mode: FFFMode) -> i64 {
        let accesses = self
            .get_accesses(file_path)
            .ok()
            .flatten()
            .unwrap_or_default();

        Self::calculate_access_score(&accesses, self.get_now(), mode)
    }

    pub fn calculate_access_score(accesses: &VecDeque<u64>, now: u64, mode: FFFMode) -> i64 {
        if accesses.is_empty() {
            return 0;
        }

        let decay_constant = if mode.is_ai() {
            AI_DECAY_CONSTANT
        } else {
            DECAY_CONSTANT
        };
        let max_history_days = if mode.is_ai() {
            AI_MAX_HISTORY_DAYS
        } else {
            MAX_HISTORY_DAYS
        };

        let mut total_frecency = 0.0;
        let cutoff_time = now.saturating_sub((max_history_days * SECONDS_PER_DAY) as u64);

        for &access_time in accesses.iter().rev() {
            if access_time < cutoff_time {
                break;
            }

            let days_ago = (now.saturating_sub(access_time) as f64) / SECONDS_PER_DAY;
            let decay_factor = (-decay_constant * days_ago).exp();
            total_frecency += decay_factor;
        }

        let normalized_frecency = if total_frecency <= 10.0 {
            total_frecency
        } else {
            10.0 + (total_frecency - 10.0).sqrt()
        };

        normalized_frecency.round() as i64
    }

    pub fn get_modification_score(
        &self,
        modified_time: u64,
        git_status: Option<git2::Status>,
        mode: FFFMode,
    ) -> i64 {
        let is_modified_git_status = git_status.is_some_and(is_modified_status);
        if !is_modified_git_status {
            return 0;
        }

        let thresholds = if mode.is_ai() {
            &AI_MODIFICATION_THRESHOLDS
        } else {
            &MODIFICATION_THRESHOLDS
        };

        let now = self.get_now();
        let duration_since = now.saturating_sub(modified_time);

        for i in 0..thresholds.len() {
            let (current_points, current_threshold) = thresholds[i];

            if duration_since <= current_threshold {
                if i == 0 || duration_since == current_threshold {
                    return current_points;
                }

                let (prev_points, prev_threshold) = thresholds[i - 1];
                let time_range = current_threshold - prev_threshold;
                let time_offset = duration_since - prev_threshold;
                let points_diff = prev_points - current_points;

                return prev_points - (points_diff * time_offset as i64) / time_range as i64;
            }
        }

        0
    }
}

fn storage_error(error: FrecencyPersistenceError) -> Error {
    Error::Persistence(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frecency_calculation() {
        let current_time = 1000000000;

        let score = FrecencyTracker::calculate_access_score(
            &VecDeque::new(),
            current_time,
            FFFMode::Neovim,
        );
        assert_eq!(score, 0);

        let accesses = VecDeque::from([current_time]);
        let score =
            FrecencyTracker::calculate_access_score(&accesses, current_time, FFFMode::Neovim);
        assert_eq!(score, 1);

        let ten_days_seconds = 10 * 86400;
        let accesses = VecDeque::from([current_time - ten_days_seconds]);
        let score =
            FrecencyTracker::calculate_access_score(&accesses, current_time, FFFMode::Neovim);
        assert_eq!(score, 1);

        let accesses = VecDeque::from([current_time, current_time - 86400, current_time - 172800]);
        let score =
            FrecencyTracker::calculate_access_score(&accesses, current_time, FFFMode::Neovim);
        assert!(score > 2 && score < 4, "Score: {score}");

        let thirty_days = 30 * 86400;
        let accesses = VecDeque::from([current_time - thirty_days]);
        let score =
            FrecencyTracker::calculate_access_score(&accesses, current_time, FFFMode::Neovim);
        assert!(
            score < 2,
            "Old access should have minimal score, got: {score}"
        );
    }

    #[test]
    fn test_track_access_updates_index_and_store() {
        let store = InMemoryFrecencyStore::new();
        let tracker = FrecencyTracker::new(store).unwrap();
        let path = Path::new("/test/project/src/main.rs");

        tracker.track_access(path).unwrap();

        assert!(tracker.get_access_score(path, FFFMode::Neovim) > 0);
        assert_eq!(tracker.store.entry_count().unwrap(), 1);
    }

    #[test]
    fn test_modification_score_interpolation() {
        let tracker = FrecencyTracker::memory_only().unwrap();

        let current_time = tracker.get_now();
        let git_status = Some(git2::Status::WT_MODIFIED);

        let five_minutes_ago = current_time - (5 * 60);
        let score = tracker.get_modification_score(five_minutes_ago, git_status, FFFMode::Neovim);
        assert_eq!(score, 15, "5 minutes should interpolate to 15 points");

        let two_minutes_ago = current_time - (2 * 60);
        let score = tracker.get_modification_score(two_minutes_ago, git_status, FFFMode::Neovim);
        assert_eq!(score, 16, "2 minutes should be exactly 16 points");

        let fifteen_minutes_ago = current_time - (15 * 60);
        let score =
            tracker.get_modification_score(fifteen_minutes_ago, git_status, FFFMode::Neovim);
        assert_eq!(score, 8, "15 minutes should be exactly 8 points");

        let twelve_hours_ago = current_time - (12 * 60 * 60);
        let score = tracker.get_modification_score(twelve_hours_ago, git_status, FFFMode::Neovim);
        assert_eq!(score, 4, "12 hours should interpolate to 4 points");

        let eighteen_hours_ago = current_time - (18 * 60 * 60);
        let score = tracker.get_modification_score(eighteen_hours_ago, git_status, FFFMode::Neovim);
        assert_eq!(score, 3, "18 hours should interpolate to 3 points");

        let score = tracker.get_modification_score(five_minutes_ago, None, FFFMode::Neovim);
        assert_eq!(score, 0, "No git status should return 0");
    }
}
