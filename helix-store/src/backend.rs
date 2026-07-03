use std::path::{Path, PathBuf};

use drizzle::sqlite::rusqlite::Drizzle;
use rusqlite::{params, Connection, Params};

use crate::migrations::{has_version_sql, insert_version_sql, migrations, version_table_sql};
use crate::schema::Schema;
use crate::{DatabaseKind, Error, Result, BUSY_TIMEOUT};

#[derive(Debug, Clone, PartialEq)]
pub enum SqliteValue {
    Integer(i64),
    Real(f64),
    Text(String),
    Null,
}

pub trait Backend {
    fn execute_batch(&mut self, sql: &str) -> Result<()>;
    fn execute<P>(&mut self, sql: &str, params: P) -> Result<usize>
    where
        P: Params;
    fn query_row<T, P, F>(&mut self, sql: &str, params: P, map: F) -> Result<T>
    where
        P: Params,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>;
}

pub(crate) type Db = drizzle::sqlite::rusqlite::Drizzle<Schema>;

pub(crate) struct DrizzleBackend {
    db: Db,
    pub(crate) schema: Schema,
}

impl DrizzleBackend {
    pub(crate) fn open(path: impl AsRef<Path>, kind: DatabaseKind) -> Result<Self> {
        prepare_parent(path.as_ref())?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let (db, schema) = Drizzle::new(conn, Schema::new());
        let mut backend = Self { db, schema };
        backend.configure_connection()?;
        backend.run_migrations(kind)?;
        Ok(backend)
    }

    pub(crate) fn db(&mut self) -> &mut Db {
        &mut self.db
    }

    pub(crate) fn conn(&self) -> &Connection {
        self.db.conn()
    }

    pub(crate) fn journal_mode(&mut self) -> Result<String> {
        self.query_row("PRAGMA journal_mode", [], |row| row.get(0))
    }

    fn configure_connection(&mut self) -> Result<()> {
        self.execute_batch(
            r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
"#,
        )?;
        self.execute_batch("PRAGMA busy_timeout = 5000")?;
        Ok(())
    }

    fn run_migrations(&mut self, kind: DatabaseKind) -> Result<()> {
        self.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            self.execute_batch(version_table_sql())?;
            for migration in migrations(kind) {
                let exists: i64 =
                    self.query_row(has_version_sql(), params![migration.version], |row| {
                        row.get(0)
                    })?;
                if exists == 0 {
                    self.execute_batch(migration.sql)?;
                    self.execute(
                        insert_version_sql(),
                        params![migration.version, migration.name],
                    )?;
                }
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(err) => {
                let _ = self.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }
}

impl Backend for DrizzleBackend {
    fn execute_batch(&mut self, sql: &str) -> Result<()> {
        self.conn().execute_batch(sql).map_err(Error::from)
    }

    fn execute<P>(&mut self, sql: &str, params: P) -> Result<usize>
    where
        P: Params,
    {
        self.conn().execute(sql, params).map_err(Error::from)
    }

    fn query_row<T, P, F>(&mut self, sql: &str, params: P, map: F) -> Result<T>
    where
        P: Params,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        self.conn().query_row(sql, params, map).map_err(Error::from)
    }
}

fn prepare_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent).map_err(|source| Error::PrepareDirectory {
        path: PathBuf::from(parent),
        source,
    })
}
