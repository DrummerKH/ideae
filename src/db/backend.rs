//! What a session asks of a database, whichever it is. A backend is used
//! from the session's own thread and may block; [`Stop`] is the one thing
//! called from elsewhere.

use std::sync::Arc;

use anyhow::Result;

use super::{
    config::{Connection, Driver, TlsMode},
    schema::Schema,
    tunnel::Tunnel,
    value::{Kind, Row},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub kind: Kind,
    /// The table's column it reads, as the server says: not guessed from a
    /// name, which an alias can make any other column's. None for an expression.
    pub source: Option<ColumnSource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnSource {
    /// Where the server tells it (MySQL can read across databases).
    pub database: Option<String>,
    pub table: String,
    pub column: String,
}

#[derive(Debug)]
pub enum DbError {
    /// The server refused this statement; the connection is fine.
    Refused(String),
    /// Refused, and it said where: a byte of the statement.
    RefusedAt(String, usize),
    /// Ended by [`Stop`].
    Interrupted,
    /// The connection is of no more use.
    Gone(String),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Refused(text) | DbError::RefusedAt(text, _) | DbError::Gone(text) => f.write_str(text),
            DbError::Interrupted => f.write_str("Stopped"),
        }
    }
}

impl DbError {
    pub fn position(&self) -> Option<usize> {
        match self {
            DbError::RefusedAt(_, at) => Some(*at),
            _ => None,
        }
    }
}

impl std::error::Error for DbError {}

pub type DbResult<T> = Result<T, DbError>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Go,
    /// Enough: the backend ends the statement on the server and returns.
    Stop,
}

/// Where a statement's results go.
pub trait Sink {
    /// A result with rows begins.
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> Flow;
    /// `bytes`: about how long its values are.
    fn row(&mut self, row: Row, bytes: usize) -> Flow;
    /// A result ended; for a statement without rows, the only call.
    fn done(&mut self, affected: u64);
}

/// Stops what the connection is running, from another thread.
pub trait Stop: Send + Sync {
    fn stop(&self) -> Result<()>;
}

pub trait Backend: Send {
    fn version(&mut self) -> String;
    fn databases(&mut self) -> DbResult<Vec<String>>;
    fn use_database(&mut self, name: &str) -> DbResult<()>;
    fn schema(&mut self, database: &str) -> DbResult<Schema>;
    /// A cheap print of what `schema` would read: the same, and it need not
    /// be read again. None where there is no such thing to ask (Redis's keys).
    fn schema_stamp(&mut self, database: &str) -> DbResult<Option<String>>;
    /// What creates this table, as the server tells it.
    fn definition(&mut self, table: &str) -> DbResult<String>;
    fn set_manual(&mut self, manual: bool) -> DbResult<()>;
    fn commit(&mut self) -> DbResult<()>;
    fn rollback(&mut self) -> DbResult<()>;
    /// Opens a transaction for what must be kept whole or not at all.
    fn begin(&mut self) -> DbResult<()>;
    /// Whether the connection still answers. Asked before a statement is
    /// sent, so that one is never lost to a connection that died idle.
    fn ping(&mut self) -> bool;
    fn run(&mut self, sql: &str, sink: &mut dyn Sink) -> DbResult<()>;
    fn stopper(&self) -> Arc<dyn Stop>;
    /// Whether what goes to the server is encrypted, as this connection turned out.
    fn encrypted(&self) -> bool;
}

/// Where to connect: the tunnel's port, or the connection's own.
pub fn address(connection: &Connection, tunnel: Option<&Tunnel>) -> (String, u16) {
    match tunnel {
        Some(tunnel) => ("127.0.0.1".to_string(), tunnel.port),
        None => (connection.host.clone(), connection.port),
    }
}

/// Whether a failure was the server turning the login down: another way of
/// connecting would only count as one more wrong try.
fn login_refused(error: &anyhow::Error) -> bool {
    let text = error.to_string();
    ["Access denied", "password authentication failed", "WRONGPASS", "NOAUTH", "no password supplied", "28P01", "1045"].iter().any(|m| text.contains(m))
}

/// `Prefer` tries encrypted, and plain only when that is what failed.
pub fn connect(connection: &Connection, tunnel: Option<&Tunnel>, password: Option<String>, database: Option<String>) -> Result<Box<dyn Backend>> {
    if connection.tls.mode != TlsMode::Prefer {
        return connect_as(connection, tunnel, password, database);
    }
    let mut strict = connection.clone();
    strict.tls.mode = TlsMode::Require;
    match connect_as(&strict, tunnel, password.clone(), database.clone()) {
        Ok(db) => Ok(db),
        Err(e) if login_refused(&e) => Err(e),
        Err(_) => {
            strict.tls.mode = TlsMode::Off;
            connect_as(&strict, tunnel, password, database)
        }
    }
}

fn connect_as(connection: &Connection, tunnel: Option<&Tunnel>, password: Option<String>, database: Option<String>) -> Result<Box<dyn Backend>> {
    let database = database.or_else(|| connection.database.clone());
    Ok(match connection.driver {
        Driver::MySql => Box::new(super::mysql_backend::MySql::connect(connection, tunnel, password, database)?),
        Driver::Postgres => Box::new(super::postgres_backend::Postgres::connect(connection, tunnel, password, database)?),
        Driver::Redis => Box::new(super::redis_backend::Redis::connect(connection, tunnel, password, database)?),
    })
}
