use crate::error::Result;

/// Health information about a database
#[derive(Debug, Clone)]
pub struct DbHealth {
    /// Path to the database file
    pub path: String,
    /// Size on disk in bytes
    pub disk_size: u64,
    /// Entry counts by table name
    pub entry_counts: Vec<(&'static str, u64)>,
}

pub trait DbHealthChecker {
    fn get_health(&self) -> Result<DbHealth>;
}
