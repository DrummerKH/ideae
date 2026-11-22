//! MySQL and MariaDB. Over a TCP port even through a tunnel: on a unix socket
//! the driver would send the password in the clear (see `tunnel`).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use mysql::prelude::Queryable as _;

use super::{
    backend::{Backend, ColumnInfo, ColumnSource, DbError, DbResult, Flow, Sink, Stop, address},
    config::{Connection, TlsMode},
    schema::{ForeignKey, IndexMeta, RoutineMeta, Schema},
    tunnel::Tunnel,
    value::{Kind, Row},
};

const CONNECT_WITHIN: Duration = Duration::from_secs(15);
/// `ER_QUERY_INTERRUPTED`: what a statement ended by `KILL QUERY` fails with.
const QUERY_INTERRUPTED: u16 = 1317;

pub struct MySql {
    conn: mysql::Conn,
    stopper: Arc<Killer>,
    encrypted: bool,
}

/// The driver's read blocks, so `KILL QUERY` goes over a second connection.
struct Killer {
    options: mysql::Opts,
    id: AtomicU32,
}

impl Stop for Killer {
    fn stop(&self) -> Result<()> {
        let mut conn = mysql::Conn::new(self.options.clone())?;
        conn.query_drop(format!("KILL QUERY {}", self.id.load(Ordering::Acquire)))?;
        Ok(())
    }
}

fn error(e: mysql::Error) -> DbError {
    match e {
        mysql::Error::MySqlError(e) if e.code == QUERY_INTERRUPTED => DbError::Interrupted,
        mysql::Error::MySqlError(e) => DbError::Refused(format!("ERROR {} ({}): {}", e.code, e.state, e.message)),
        other => DbError::Gone(other.to_string()),
    }
}

/// Where MySQL's "near '…' at line N" points in the statement.
fn placed(error: DbError, sql: &str) -> DbError {
    let DbError::Refused(text) = error else { return error };
    match position_of(&text, sql) {
        Some(at) => DbError::RefusedAt(text, at),
        None => DbError::Refused(text),
    }
}

fn position_of(message: &str, sql: &str) -> Option<usize> {
    let (_, rest) = message.split_once("near '")?;
    let (near, line) = rest.rsplit_once("' at line ")?;
    let line: usize = line.trim().parse().ok()?;
    let start = sql.split_inclusive('\n').take(line.checked_sub(1)?).map(str::len).sum::<usize>();
    // The server quotes at most some 80 characters of it.
    let head: String = near.chars().take(24).collect();
    if head.is_empty() {
        return Some(sql.len());
    }
    sql.get(start..)?.find(&head).map(|p| start + p)
}

/// The real table and column, past any alias; an expression has none.
fn source_of(column: &mysql::Column) -> Option<ColumnSource> {
    let (table, name) = (column.org_table_str(), column.org_name_str());
    if table.is_empty() || name.is_empty() {
        return None;
    }
    let database = column.schema_str();
    Some(ColumnSource { database: (!database.is_empty()).then(|| database.into_owned()), table: table.into_owned(), column: name.into_owned() })
}

impl MySql {
    pub fn connect(connection: &Connection, tunnel: Option<&Tunnel>, password: Option<String>, database: Option<String>) -> Result<Self> {
        let (host, port) = address(connection, tunnel);
        let ssl = match connection.tls.mode {
            TlsMode::Off => None,
            TlsMode::Prefer | TlsMode::Require => Some(mysql::SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)),
            // Through a tunnel the driver can only check who signed it: its
            // name would be compared with this machine's.
            TlsMode::Verify => {
                let ssl = mysql::SslOpts::default().with_danger_skip_domain_validation(tunnel.is_some());
                Some(match &connection.tls.ca {
                    Some(ca) => ssl.with_root_cert_path(Some(ca.clone())),
                    None => ssl,
                })
            }
        };
        let options: mysql::Opts = mysql::OptsBuilder::new()
            .ssl_opts(ssl)
            .ip_or_hostname(Some(host))
            .tcp_port(port)
            .user(Some(connection.user.clone()))
            .pass(password)
            .db_name(database)
            // Left on, the driver asks the server for its socket and goes there
            // on this machine: over a tunnel, that could be another database.
            .prefer_socket(false)
            .tcp_connect_timeout(Some(CONNECT_WITHIN))
            // A link that died in the machine's sleep is found out, not waited on.
            .tcp_keepalive_time_ms(Some(30_000))
            .into();
        let conn = mysql::Conn::new(options.clone()).map_err(|e| anyhow!("MySQL: {e}"))?;
        let stopper = Arc::new(Killer { options, id: AtomicU32::new(conn.connection_id()) });
        Ok(MySql { conn, stopper, encrypted: connection.tls.mode != TlsMode::Off })
    }
}

impl Backend for MySql {
    fn version(&mut self) -> String {
        self.conn.query_first::<String, _>("SELECT VERSION()").ok().flatten().unwrap_or_default()
    }

    fn databases(&mut self) -> DbResult<Vec<String>> {
        self.conn.query::<String, _>("SHOW DATABASES").map_err(error)
    }

    fn use_database(&mut self, name: &str) -> DbResult<()> {
        self.conn.select_db(name).map_err(error)
    }

    fn schema(&mut self, database: &str) -> DbResult<Schema> {
        const COLUMNS: &str = "SELECT c.TABLE_NAME, t.TABLE_TYPE, c.COLUMN_NAME, c.COLUMN_TYPE, c.IS_NULLABLE, c.COLUMN_KEY \
            FROM information_schema.COLUMNS c JOIN information_schema.TABLES t \
            ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
            WHERE c.TABLE_SCHEMA = ? ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION";
        let rows = self.conn.exec::<(String, String, String, String, String, String), _, _>(COLUMNS, (database,)).map_err(error)?;
        let mut schema = Schema::from_rows(database.to_string(), rows);
        const KEYS: &str = "SELECT TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
            WHERE TABLE_SCHEMA = ? AND REFERENCED_TABLE_SCHEMA = TABLE_SCHEMA AND REFERENCED_TABLE_NAME IS NOT NULL";
        // Without them only the suggestions for a JOIN are poorer.
        if let Ok(keys) = self.conn.exec::<(String, String, String, String), _, _>(KEYS, (database,)) {
            schema.foreign_keys = keys.into_iter().map(|(table, column, to_table, to_column)| ForeignKey { table, column, to_table, to_column }).collect();
        }
        const INDEXES: &str = "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX";
        if let Ok(rows) = self.conn.exec::<(String, String, i64, Option<String>), _, _>(INDEXES, (database,)) {
            for (table, name, non_unique, column) in rows {
                if schema.indexes.last().is_none_or(|i: &IndexMeta| i.table != table || i.name != name) {
                    schema.indexes.push(IndexMeta { table, name, columns: Vec::new(), unique: non_unique == 0 });
                }
                if let (Some(index), Some(column)) = (schema.indexes.last_mut(), column) {
                    index.columns.push(column);
                }
            }
        }
        const ROUTINES: &str = "SELECT ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? ORDER BY 1";
        if let Ok(rows) = self.conn.exec::<(String, String), _, _>(ROUTINES, (database,)) {
            schema.routines = rows.into_iter().map(|(name, kind)| RoutineMeta { name, kind: kind.to_lowercase() }).collect();
        }
        Ok(schema)
    }

    fn schema_stamp(&mut self, database: &str) -> DbResult<Option<String>> {
        const STAMP: &str = "SELECT CONCAT_WS('/', \
            (SELECT CONCAT(COUNT(*), ':', COALESCE(BIT_XOR(CRC32(CONCAT_WS('|', TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, ORDINAL_POSITION))), 0)) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ?), \
            (SELECT CONCAT(COUNT(*), ':', COALESCE(BIT_XOR(CRC32(CONCAT_WS('|', TABLE_NAME, INDEX_NAME, COLUMN_NAME, SEQ_IN_INDEX, NON_UNIQUE))), 0)) FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ?), \
            (SELECT CONCAT(COUNT(*), ':', COALESCE(BIT_XOR(CRC32(CONCAT_WS('|', TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME))), 0)) FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = ?), \
            (SELECT CONCAT(COUNT(*), ':', COALESCE(BIT_XOR(CRC32(CONCAT_WS('|', ROUTINE_NAME, ROUTINE_TYPE))), 0)) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ?))";
        self.conn.exec_first::<String, _, _>(STAMP, (database, database, database, database)).map_err(error)
    }

    fn definition(&mut self, table: &str) -> DbResult<String> {
        let quoted = format!("`{}`", table.replace('`', "``"));
        let row = self.conn.query_first::<mysql::Row, _>(format!("SHOW CREATE TABLE {quoted}")).map_err(error)?;
        Ok(row.and_then(|r| r.get::<String, _>(1)).unwrap_or_default())
    }

    fn set_manual(&mut self, manual: bool) -> DbResult<()> {
        self.conn.query_drop(if manual { "SET autocommit = 0" } else { "SET autocommit = 1" }).map_err(error)
    }

    fn commit(&mut self) -> DbResult<()> {
        self.conn.query_drop("COMMIT").map_err(error)
    }

    fn rollback(&mut self) -> DbResult<()> {
        self.conn.query_drop("ROLLBACK").map_err(error)
    }

    fn begin(&mut self) -> DbResult<()> {
        self.conn.query_drop("START TRANSACTION").map_err(error)
    }

    fn ping(&mut self) -> bool {
        self.conn.ping().is_ok()
    }

    fn run(&mut self, sql: &str, sink: &mut dyn Sink) -> DbResult<()> {
        let stopper = self.stopper.clone();
        // Enough rows shown is no reason to stop a statement that changes data.
        let may_stop = !super::statement::writes(sql);
        let mut result = self.conn.query_iter(sql).map_err(|e| placed(error(e), sql))?;
        while let Some(mut set) = result.iter() {
            let kinds: Vec<Kind> = set.columns().as_ref().iter().map(|c| Kind::of(c.column_type(), c.flags(), c.character_set())).collect();
            let columns: Vec<ColumnInfo> = set.columns().as_ref().iter().zip(&kinds).map(|(c, kind)| ColumnInfo { name: c.name_str().into_owned(), kind: *kind, source: source_of(c) }).collect();
            // Read before the set is: past its end, the driver no longer has it.
            let affected = set.affected_rows();
            let mut enough = !columns.is_empty() && sink.columns(columns) == Flow::Stop;
            if !enough {
                for row in set.by_ref() {
                    let mut bytes = 0;
                    let row: Row = row
                        .map_err(|e| placed(error(e), sql))?
                        .unwrap()
                        .into_iter()
                        .zip(&kinds)
                        .map(|(value, kind)| {
                            if let mysql::Value::Bytes(b) = &value {
                                bytes += b.len();
                            }
                            kind.cell(value)
                        })
                        .collect();
                    if sink.row(row, bytes) == Flow::Stop {
                        enough = true;
                        break;
                    }
                }
            }
            sink.done(affected);
            if enough && may_stop {
                // Dropped unread, a result is read to its end: the server stops first.
                let _ = stopper.stop();
                break;
            }
        }
        drop(result);
        Ok(())
    }

    fn stopper(&self) -> Arc<dyn Stop> {
        self.stopper.clone()
    }

    fn encrypted(&self) -> bool {
        self.encrypted
    }
}

#[cfg(test)]
mod tests {
    use super::position_of;

    #[test]
    fn an_error_is_placed_where_the_server_says() {
        let sql = "select a\nfrom t\nwher b = 1";
        let message = "ERROR 1064 (42000): You have an error in your SQL syntax; check the manual near 'wher b = 1' at line 3";
        assert_eq!(position_of(message, sql), Some(sql.find("wher").unwrap()));
        assert_eq!(position_of("ERROR 1146: no such table", sql), None);
    }
}
