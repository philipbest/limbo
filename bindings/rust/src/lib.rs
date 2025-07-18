//! # Turso bindings for Rust
//!
//! Turso is an in-process SQL database engine, compatible with SQLite.
//!
//! ## Getting Started
//!
//! To get started, you first need to create a [`Database`] object and then open a [`Connection`] to it, which you use to query:
//!
//! ```rust,no_run
//! # async fn run() {
//! use turso::Builder;
//!
//! let db = Builder::new_local(":memory:").build().await.unwrap();
//! let conn = db.connect().unwrap();
//! conn.execute("CREATE TABLE IF NOT EXISTS users (email TEXT)", ()).await.unwrap();
//! conn.execute("INSERT INTO users (email) VALUES ('alice@example.org')", ()).await.unwrap();
//! # }
//! ```
//!
//! You can also prepare statements with the [`Connection`] object and then execute the [`Statement`] objects:
//!
//! ```rust,no_run
//! # use crate::turso::futures_util::TryStreamExt;
//! # async fn run() {
//! # use turso::Builder;
//! # let db = Builder::new_local(":memory:").build().await.unwrap();
//! # let conn = db.connect().unwrap();
//! let mut stmt = conn.prepare("SELECT * FROM users WHERE email = ?1").await.unwrap();
//! let mut rows = stmt.query(["foo@example.com"]).await.unwrap();
//! let row = rows.try_next().await.unwrap().unwrap();
//! let value = row.get_value(0).unwrap();
//! println!("Row: {:?}", value);
//! # }
//! ```

pub mod params;
pub mod row;
pub mod statement;
pub mod value;

#[cfg(feature = "futures")]
pub use futures_util;
pub use params::params_from_iter;
pub use value::Value;

use crate::params::*;
use crate::row::{Row, Rows};
use crate::statement::Statement;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("SQL conversion failure: `{0}`")]
    ToSqlConversionFailure(BoxError),
    #[error("Mutex lock error: {0}")]
    MutexError(String),
    #[error("SQL execution failure: `{0}`")]
    SqlExecutionFailure(String),
}

impl From<turso_core::LimboError> for Error {
    fn from(err: turso_core::LimboError) -> Self {
        Error::SqlExecutionFailure(err.to_string())
    }
}

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A builder for `Database`.
pub struct Builder {
    path: String,
}

impl Builder {
    /// Create a new local database.
    pub fn new_local(path: &str) -> Self {
        Self {
            path: path.to_string(),
        }
    }

    /// Build the database.
    #[allow(unused_variables, clippy::arc_with_non_send_sync)]
    pub async fn build(self) -> Result<Database> {
        match self.path.as_str() {
            ":memory:" => {
                let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::MemoryIO::new());
                let db = turso_core::Database::open_file(
                    io,
                    self.path.as_str(),
                    false,
                    indexes_enabled(),
                )?;
                Ok(Database { inner: db })
            }
            path => {
                let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::PlatformIO::new()?);
                let db = turso_core::Database::open_file(io, path, false, indexes_enabled())?;
                Ok(Database { inner: db })
            }
        }
    }
}

fn indexes_enabled() -> bool {
    #[cfg(feature = "experimental_indexes")]
    return true;
    #[cfg(not(feature = "experimental_indexes"))]
    return false;
}

/// A database.
///
/// The `Database` object points to a database and allows you to connect to it
#[derive(Clone)]
pub struct Database {
    inner: Arc<turso_core::Database>,
}

unsafe impl Send for Database {}
unsafe impl Sync for Database {}

impl Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").finish()
    }
}

impl Database {
    /// Connect to the database.
    pub fn connect(&self) -> Result<Connection> {
        let conn = self.inner.connect()?;
        #[allow(clippy::arc_with_non_send_sync)]
        let connection = Connection {
            inner: Arc::new(Mutex::new(conn)),
        };
        Ok(connection)
    }
}

/// A database connection.
pub struct Connection {
    inner: Arc<Mutex<Arc<turso_core::Connection>>>,
}

impl Clone for Connection {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

unsafe impl Send for Connection {}
unsafe impl Sync for Connection {}

impl Connection {
    /// Query the database with SQL.
    pub async fn query(&self, sql: &str, params: impl IntoParams) -> Result<Rows> {
        let mut stmt = self.prepare(sql).await?;
        stmt.query(params).await
    }

    /// Execute SQL statement on the database.
    pub async fn execute(&self, sql: &str, params: impl IntoParams) -> Result<u64> {
        let mut stmt = self.prepare(sql).await?;
        stmt.execute(params).await
    }

    /// Prepare a SQL statement for later execution.
    pub async fn prepare(&self, sql: &str) -> Result<Statement> {
        let conn = self
            .inner
            .lock()
            .map_err(|e| Error::MutexError(e.to_string()))?;

        let stmt = conn.prepare(sql)?;

        #[allow(clippy::arc_with_non_send_sync)]
        let statement = Statement {
            inner: Arc::new(Mutex::new(stmt)),
        };
        Ok(statement)
    }

    /// Query a pragma.
    pub fn pragma_query<F>(&self, pragma_name: &str, mut f: F) -> Result<()>
    where
        F: FnMut(&Row) -> turso_core::Result<()>,
    {
        let conn = self
            .inner
            .lock()
            .map_err(|e| Error::MutexError(e.to_string()))?;

        let rows: Vec<Row> = conn
            .pragma_query(pragma_name)
            .map_err(|e| Error::SqlExecutionFailure(e.to_string()))?
            .iter()
            .map(|row| row.iter().collect::<Row>())
            .collect();

        rows.iter().try_for_each(|row| {
            f(row).map_err(|e| {
                Error::SqlExecutionFailure(format!("Error executing user defined function: {}", e))
            })
        })?;
        Ok(())
    }
}

impl Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").finish()
    }
}

/// Column information.
pub struct Column {
    name: String,
    decl_type: Option<String>,
}

impl Column {
    /// Return the name of the column.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the type of the column.
    pub fn decl_type(&self) -> Option<&str> {
        self.decl_type.as_deref()
    }
}

pub trait IntoValue {
    fn into_value(self) -> Result<Value>;
}

#[derive(Debug, Clone)]
pub enum Params {
    None,
    Positional(Vec<Value>),
    Named(Vec<(String, Value)>),
}

pub struct Transaction {}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "futures")]
    use futures_util::TryStreamExt;
    use tempfile::NamedTempFile;

    #[cfg(not(feature = "futures"))]
    macro_rules! rows_next {
        ($rows:expr) => {
            $rows.next()
        };
    }

    #[cfg(feature = "futures")]
    macro_rules! rows_next {
        ($rows:expr) => {
            $rows.try_next()
        };
    }

    #[tokio::test]
    async fn test_database_persistence() -> Result<()> {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap();

        // First, create the database, a table, and insert some data
        {
            let db = Builder::new_local(db_path).build().await?;
            let conn = db.connect()?;
            conn.execute(
                "CREATE TABLE test_persistence (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
                (),
            )
            .await?;
            conn.execute("INSERT INTO test_persistence (name) VALUES ('Alice');", ())
                .await?;
            conn.execute("INSERT INTO test_persistence (name) VALUES ('Bob');", ())
                .await?;
        } // db and conn are dropped here, simulating closing

        // Now, re-open the database and check if the data is still there
        let db = Builder::new_local(db_path).build().await?;
        let conn = db.connect()?;

        let mut rows = conn
            .query("SELECT name FROM test_persistence ORDER BY id;", ())
            .await?;

        let row1 = rows_next!(rows).await?.expect("Expected first row");
        assert_eq!(row1.get_value(0)?, Value::Text("Alice".to_string()));

        let row2 = rows_next!(rows).await?.expect("Expected second row");
        assert_eq!(row2.get_value(0)?, Value::Text("Bob".to_string()));

        assert!(rows_next!(rows).await?.is_none(), "Expected no more rows");

        Ok(())
    }

    #[tokio::test]
    async fn test_database_persistence_many_frames() -> Result<()> {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap();

        const NUM_INSERTS: usize = 100;
        const TARGET_STRING_LEN: usize = 1024; // 1KB

        let mut original_data = Vec::with_capacity(NUM_INSERTS);
        for i in 0..NUM_INSERTS {
            let prefix = format!("test_string_{:04}_", i);
            let padding_len = TARGET_STRING_LEN.saturating_sub(prefix.len());
            let padding: String = "A".repeat(padding_len);
            original_data.push(format!("{}{}", prefix, padding));
        }

        // First, create the database, a table, and insert many large strings
        {
            let db = Builder::new_local(db_path).build().await?;
            let conn = db.connect()?;
            conn.execute(
                "CREATE TABLE test_large_persistence (id INTEGER PRIMARY KEY AUTOINCREMENT, data TEXT NOT NULL);",
                (),
            )
            .await?;

            for data_val in &original_data {
                conn.execute(
                    "INSERT INTO test_large_persistence (data) VALUES (?);",
                    params::Params::Positional(vec![Value::Text(data_val.clone())]),
                )
                .await?;
            }
        } // db and conn are dropped here, simulating closing

        // Now, re-open the database and check if the data is still there
        let db = Builder::new_local(db_path).build().await?;
        let conn = db.connect()?;

        let mut rows = conn
            .query("SELECT data FROM test_large_persistence ORDER BY id;", ())
            .await?;

        for (i, value) in original_data.iter().enumerate().take(NUM_INSERTS) {
            let row = rows_next!(rows)
                .await?
                .unwrap_or_else(|| panic!("Expected row {} but found None", i));
            assert_eq!(
                row.get_value(0)?,
                Value::Text(value.clone()),
                "Mismatch in retrieved data for row {}",
                i
            );
        }

        assert!(
            rows_next!(rows).await?.is_none(),
            "Expected no more rows after retrieving all inserted data"
        );

        // Delete the WAL file only and try to re-open and query
        let wal_path = format!("{}-wal", db_path);
        std::fs::remove_file(&wal_path)
            .map_err(|e| eprintln!("Warning: Failed to delete WAL file for test: {}", e))
            .unwrap();

        // Attempt to re-open the database after deleting WAL and assert that table is missing.
        let db_after_wal_delete = Builder::new_local(db_path).build().await?;
        let conn_after_wal_delete = db_after_wal_delete.connect()?;

        let query_result_after_wal_delete = conn_after_wal_delete
            .query("SELECT data FROM test_large_persistence ORDER BY id;", ())
            .await;

        match query_result_after_wal_delete {
            Ok(_) => panic!("Query succeeded after WAL deletion and DB reopen, but was expected to fail because the table definition should have been in the WAL."),
            Err(Error::SqlExecutionFailure(msg)) => {
                assert!(
                    msg.contains("test_large_persistence not found"),
                    "Expected 'test_large_persistence not found' error, but got: {}",
                    msg
                );
            }
            Err(e) => panic!(
                "Expected SqlExecutionFailure for 'no such table', but got a different error: {:?}",
                e
            ),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_database_persistence_write_one_frame_many_times() -> Result<()> {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap();

        for i in 0..100 {
            {
                let db = Builder::new_local(db_path).build().await?;
                let conn = db.connect()?;

                conn.execute("CREATE TABLE IF NOT EXISTS test_persistence (id INTEGER PRIMARY KEY, name TEXT NOT NULL);", ()).await?;
                conn.execute("INSERT INTO test_persistence (name) VALUES ('Alice');", ())
                    .await?;
            }
            {
                let db = Builder::new_local(db_path).build().await?;
                let conn = db.connect()?;

                let mut rows_iter = conn
                    .query("SELECT count(*) FROM test_persistence;", ())
                    .await?;
                let rows = rows_next!(rows_iter).await?.unwrap();
                assert_eq!(rows.get_value(0)?, Value::Integer(i as i64 + 1));
                assert!(rows_next!(rows_iter).await?.is_none());
            }
        }

        Ok(())
    }
}
