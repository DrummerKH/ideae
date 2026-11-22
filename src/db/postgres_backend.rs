//! PostgreSQL. What only reads comes through `COPY … TO STDOUT`: it streams,
//! and its text is the server's own for every type, numeric and timestamps
//! included, with no decoder to keep for each.

use std::{io::BufRead as _, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use postgres::{Client, NoTls, SimpleQueryMessage, config::SslMode, types::Type};
use postgres_rustls::MakeTlsConnector;

use super::{
    backend::{Backend, ColumnInfo, ColumnSource, DbError, DbResult, Flow, Sink, Stop, address},
    config::{Connection, TlsMode},
    schema::{ForeignKey, IndexMeta, RoutineMeta, Schema},
    tunnel::Tunnel,
    value::{Cell, Kind, Row},
};

const CONNECT_WITHIN: Duration = Duration::from_secs(15);
/// `query_canceled`.
const CANCELED: &str = "57014";

pub struct Postgres {
    client: Client,
    config: postgres::Config,
    /// None for a plain connection.
    connector: Option<MakeTlsConnector>,
    stopper: Arc<Canceller>,
    manual: bool,
}

struct Canceller {
    token: std::sync::Mutex<postgres::CancelToken>,
    /// A server that takes only encrypted connections takes only encrypted cancels.
    connector: Option<MakeTlsConnector>,
}

fn open(config: &postgres::Config, connector: &Option<MakeTlsConnector>) -> Result<Client, postgres::Error> {
    match connector {
        Some(tls) => config.connect(tls.clone()),
        None => config.connect(NoTls),
    }
}

impl Stop for Canceller {
    fn stop(&self) -> Result<()> {
        let token = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match &self.connector {
            Some(tls) => token.cancel_query(tls.clone())?,
            None => token.cancel_query(NoTls)?,
        }
        Ok(())
    }
}

fn error(e: postgres::Error) -> DbError {
    match e.as_db_error() {
        Some(db) if db.code().code() == CANCELED => DbError::Interrupted,
        Some(db) => {
            let hint = db.hint().map(|h| format!("\nHint: {h}")).unwrap_or_default();
            let text = format!("ERROR {}: {}{hint}", db.code().code(), db.message());
            match db.position() {
                // In characters, from one: made a byte when the statement is at hand.
                Some(postgres::error::ErrorPosition::Original(at)) => DbError::RefusedAt(text, (*at as usize).saturating_sub(1)),
                _ => DbError::Refused(text),
            }
        }
        None if e.is_closed() => DbError::Gone("The connection was closed".into()),
        None => DbError::Gone(e.to_string()),
    }
}

fn kind_of(column: &Type) -> Kind {
    match *column {
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => Kind::Int { unsigned: false },
        Type::NUMERIC | Type::MONEY => Kind::Decimal,
        Type::FLOAT4 | Type::FLOAT8 => Kind::Float,
        Type::BYTEA => Kind::Binary,
        _ => Kind::Text,
    }
}

/// A value of `COPY`'s text format, its escapes undone; None for `\N`.
fn unescaped(field: &str) -> Option<String> {
    if field == "\\N" {
        return None;
    }
    if !field.contains('\\') {
        return Some(field.to_string());
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('v') => out.push('\u{b}'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    Some(out)
}

/// A cell from the server's text for it.
fn cell(kind: Kind, text: Option<String>) -> Cell {
    let Some(text) = text else { return Cell::Null };
    match kind {
        // `\x…` is how the server writes bytes.
        Kind::Binary => match text.strip_prefix("\\x").and_then(from_hex) {
            Some(bytes) => Cell::Bytes(bytes.into()),
            None => Cell::Text(text.into()),
        },
        kind => kind.cell(mysql::Value::Bytes(text.into_bytes())),
    }
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

/// Whether `COPY (…)` can carry it: plain reads, not `SHOW`, `EXPLAIN` or `RETURNING`.
fn copies(sql: &str) -> bool {
    let lower = sql.trim_start().to_ascii_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    matches!(first, "select" | "values" | "table" | "with") && !lower.split_whitespace().any(|w| w == "into") || (matches!(first, "insert" | "update" | "delete" | "merge") && super::statement::top_words(sql).iter().any(|w| w == "returning"))
}

impl Postgres {
    pub fn connect(connection: &Connection, tunnel: Option<&Tunnel>, password: Option<String>, database: Option<String>) -> Result<Self> {
        let (host, port) = address(connection, tunnel);
        let mut config = Client::configure();
        config.host(&host).port(port).user(&connection.user).connect_timeout(CONNECT_WITHIN).application_name("ideae");
        if let Some(password) = password.filter(|p| !p.is_empty()) {
            config.password(password);
        }
        config.dbname(database.as_deref().unwrap_or("postgres"));
        let connector = match connection.tls.mode {
            TlsMode::Off => {
                config.ssl_mode(SslMode::Disable);
                None
            }
            mode => {
                config.ssl_mode(SslMode::Require);
                let tls = super::tls::client_config(mode == TlsMode::Verify, connection.tls.ca.as_deref(), &connection.host)?;
                Some(MakeTlsConnector::new(tokio_rustls::TlsConnector::from(Arc::new(tls))))
            }
        };
        let client = open(&config, &connector).map_err(|e| anyhow!("PostgreSQL: {}", describe(&e)))?;
        let stopper = Arc::new(Canceller { token: std::sync::Mutex::new(client.cancel_token()), connector: connector.clone() });
        Ok(Postgres { client, config, connector, stopper, manual: false })
    }

    fn batch(&mut self, sql: &str) -> DbResult<()> {
        self.client.batch_execute(sql).map_err(error)
    }

    /// How many rows came. Past what the table keeps, a statement that
    /// writes is read to its end: stopping it would undo the write.
    fn copy(&mut self, sql: &str, kinds: &[Kind], sink: &mut dyn Sink) -> DbResult<u64> {
        let writes = super::statement::writes(sql);
        let mut count = 0u64;
        let statement = sql.trim().trim_end_matches(';');
        let reader = self.client.copy_out(format!("COPY ({statement}) TO STDOUT").as_str()).map_err(error)?;
        let mut reader = std::io::BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => return Ok(count),
                Ok(_) => {}
                Err(e) => {
                    return Err(match e.get_ref().and_then(|inner| inner.downcast_ref::<postgres::Error>()) {
                        Some(inner) if inner.as_db_error().is_some_and(|d| d.code().code() == CANCELED) => DbError::Interrupted,
                        Some(inner) if inner.as_db_error().is_some() => DbError::Refused(describe(inner)),
                        _ => DbError::Gone(e.to_string()),
                    });
                }
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line);
            let row: Row = text.split('\t').zip(kinds).map(|(field, kind)| cell(*kind, unescaped(field))).collect();
            count += 1;
            if sink.row(row, line.len()) == Flow::Stop {
                // Dropped unread, the reader would wait for the rest of it.
                if !writes {
                    let _ = self.stopper.stop();
                }
                let mut rest = Vec::new();
                loop {
                    rest.clear();
                    match reader.read_until(b'\n', &mut rest) {
                        Ok(0) => break,
                        Ok(_) => count += 1,
                        Err(e) if writes => {
                            return Err(match e.get_ref().and_then(|inner| inner.downcast_ref::<postgres::Error>()) {
                                Some(inner) if inner.as_db_error().is_some_and(|d| d.code().code() == CANCELED) => DbError::Interrupted,
                                Some(inner) if inner.as_db_error().is_some() => DbError::Refused(describe(inner)),
                                _ => DbError::Gone(e.to_string()),
                            });
                        }
                        Err(_) => break,
                    }
                }
                return Ok(count);
            }
        }
    }

    /// The tables and columns the server says a result's columns read.
    /// A view's or an expression's have none to change.
    fn sources(&mut self, places: &[(Option<u32>, Option<i16>)]) -> Vec<Option<ColumnSource>> {
        const NAMES: &str = "SELECT c.oid, a.attnum, CASE WHEN n.nspname = current_schema() THEN c.relname ELSE n.nspname || '.' || c.relname END, a.attname \
            FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace JOIN pg_attribute a ON a.attrelid = c.oid \
            WHERE c.oid = ANY($1) AND a.attnum > 0 AND NOT a.attisdropped";
        let mut oids: Vec<u32> = places.iter().filter_map(|p| p.0).collect();
        oids.sort_unstable();
        oids.dedup();
        let mut found = std::collections::HashMap::new();
        if !oids.is_empty()
            && let Ok(rows) = self.client.query(NAMES, &[&oids])
        {
            for row in rows {
                let (oid, number, table, column): (u32, i16, String, String) = (row.get(0), row.get(1), row.get(2), row.get(3));
                found.insert((oid, number), (table, column));
            }
        }
        places
            .iter()
            .map(|place| {
                let (table, column) = found.get(&(place.0?, place.1?))?;
                Some(ColumnSource { database: None, table: table.clone(), column: column.clone() })
            })
            .collect()
    }

    /// `SHOW`, `EXPLAIN`, `RETURNING`: short, and as the server gives them.
    fn simple(&mut self, sql: &str, sink: &mut dyn Sink) -> DbResult<()> {
        let messages = self.client.simple_query(sql).map_err(error)?;
        let (mut began, mut enough) = (false, false);
        for message in messages {
            match message {
                SimpleQueryMessage::RowDescription(columns) => {
                    began = true;
                    enough = sink.columns(columns.iter().map(|c| ColumnInfo { name: c.name().to_string(), kind: Kind::Text, source: None }).collect()) == Flow::Stop;
                }
                SimpleQueryMessage::Row(row) if !enough => {
                    let cells: Row = (0..row.len()).map(|i| cell(Kind::Text, row.get(i).map(String::from))).collect();
                    let bytes = cells.iter().map(|c| c.shown().len()).sum();
                    enough = sink.row(cells, bytes) == Flow::Stop;
                }
                SimpleQueryMessage::CommandComplete(count) => {
                    sink.done(count);
                    began = false;
                }
                _ => {}
            }
        }
        if began {
            sink.done(0);
        }
        Ok(())
    }
}

fn describe(e: &postgres::Error) -> String {
    e.as_db_error().map_or_else(|| e.to_string(), |d| format!("ERROR {}: {}", d.code().code(), d.message()))
}

impl Backend for Postgres {
    fn version(&mut self) -> String {
        self.client.query_one("SHOW server_version", &[]).ok().and_then(|r| r.try_get::<_, String>(0).ok()).unwrap_or_default()
    }

    fn databases(&mut self) -> DbResult<Vec<String>> {
        let rows = self.client.query("SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY 1", &[]).map_err(error)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// A connection is to one database: another means connecting anew.
    fn use_database(&mut self, name: &str) -> DbResult<()> {
        let mut config = self.config.clone();
        config.dbname(name);
        let client = open(&config, &self.connector).map_err(|e| DbError::Refused(describe(&e)))?;
        *self.stopper.token.lock().unwrap_or_else(|e| e.into_inner()) = client.cancel_token();
        self.client = client;
        self.config = config;
        if self.manual {
            self.batch("BEGIN")?;
        }
        Ok(())
    }

    fn schema(&mut self, database: &str) -> DbResult<Schema> {
        const COLUMNS: &str = "SELECT CASE WHEN c.table_schema = current_schema() THEN c.table_name ELSE c.table_schema || '.' || c.table_name END, \
            t.table_type, c.column_name, \
            CASE WHEN c.character_maximum_length IS NOT NULL THEN c.data_type || '(' || c.character_maximum_length || ')' ELSE c.data_type END, \
            c.is_nullable, \
            CASE WHEN EXISTS (SELECT 1 FROM information_schema.table_constraints k JOIN information_schema.key_column_usage u \
                ON u.constraint_name = k.constraint_name AND u.constraint_schema = k.constraint_schema \
                WHERE k.constraint_type = 'PRIMARY KEY' AND k.table_schema = c.table_schema AND k.table_name = c.table_name AND u.column_name = c.column_name) \
            THEN 'PRI' ELSE '' END \
            FROM information_schema.columns c JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
            WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') \
            ORDER BY 1, c.ordinal_position";
        let rows = self.client.query(COLUMNS, &[]).map_err(error)?;
        let rows = rows.iter().map(|r| {
            let text = |i: usize| r.try_get::<_, String>(i).unwrap_or_default();
            (text(0), text(1), text(2), text(3), text(4), text(5))
        });
        let mut schema = Schema::from_rows(database.to_string(), rows.collect::<Vec<_>>());
        const KEYS: &str = "SELECT CASE WHEN n.nspname = current_schema() THEN c.relname ELSE n.nspname || '.' || c.relname END, a.attname, \
            CASE WHEN rn.nspname = current_schema() THEN r.relname ELSE rn.nspname || '.' || r.relname END, ra.attname \
            FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
            JOIN pg_class r ON r.oid = k.confrelid JOIN pg_namespace rn ON rn.oid = r.relnamespace \
            JOIN LATERAL unnest(k.conkey, k.confkey) AS pair(here, there) ON true \
            JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = pair.here \
            JOIN pg_attribute ra ON ra.attrelid = k.confrelid AND ra.attnum = pair.there \
            WHERE k.contype = 'f' AND n.nspname NOT IN ('pg_catalog', 'information_schema')";
        // Without them only the suggestions for a JOIN are poorer.
        if let Ok(keys) = self.client.query(KEYS, &[]) {
            schema.foreign_keys = keys.iter().map(|r| ForeignKey { table: r.get(0), column: r.get(1), to_table: r.get(2), to_column: r.get(3) }).collect();
        }
        const INDEXES: &str = "SELECT CASE WHEN n.nspname = current_schema() THEN t.relname ELSE n.nspname || '.' || t.relname END, i.relname, x.indisunique, \
            ARRAY(SELECT pg_get_indexdef(x.indexrelid, k, true) FROM generate_series(1, x.indnkeyatts) AS k ORDER BY k) \
            FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_class t ON t.oid = x.indrelid JOIN pg_namespace n ON n.oid = t.relnamespace \
            WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') ORDER BY 1, 2";
        if let Ok(rows) = self.client.query(INDEXES, &[]) {
            schema.indexes = rows.iter().map(|r| IndexMeta { table: r.get(0), name: r.get(1), unique: r.get(2), columns: r.get(3) }).collect();
        }
        const ROUTINES: &str = "SELECT CASE WHEN n.nspname = current_schema() THEN p.proname ELSE n.nspname || '.' || p.proname END, \
            CASE p.prokind WHEN 'p' THEN 'procedure' WHEN 'a' THEN 'aggregate' WHEN 'w' THEN 'window' ELSE 'function' END \
            FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
            WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid AND d.deptype = 'e') ORDER BY 1";
        if let Ok(rows) = self.client.query(ROUTINES, &[]) {
            schema.routines = rows.iter().map(|r| RoutineMeta { name: r.get(0), kind: r.get(1) }).collect();
        }
        Ok(schema)
    }

    fn schema_stamp(&mut self, _: &str) -> DbResult<Option<String>> {
        const STAMP: &str = "SELECT concat_ws('/', \
            (SELECT count(*) || ':' || coalesce(sum(hashtext(c.oid::text || '|' || c.relname || '|' || c.relkind::text || '|' || a.attname || '|' || a.atttypid::text || '|' || a.atttypmod::text || '|' || a.attnum::text || '|' || a.attnotnull::text)::bigint), 0) \
                FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace JOIN pg_attribute a ON a.attrelid = c.oid \
                WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') AND a.attnum > 0 AND NOT a.attisdropped), \
            (SELECT count(*) || ':' || coalesce(sum(hashtext(indexrelid::text || indkey::text || indisunique::text)::bigint), 0) FROM pg_index), \
            (SELECT count(*) || ':' || coalesce(sum(hashtext(oid::text || conkey::text || coalesce(confkey::text, ''))::bigint), 0) FROM pg_constraint), \
            (SELECT count(*) || ':' || coalesce(sum(hashtext(oid::text || proname)::bigint), 0) FROM pg_proc))";
        Ok(Some(self.client.query_one(STAMP, &[]).map_err(error)?.get(0)))
    }

    /// PostgreSQL has no `SHOW CREATE TABLE`: put together from its catalog.
    fn definition(&mut self, table: &str) -> DbResult<String> {
        const COLUMNS: &str = "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, pg_get_expr(d.adbin, d.adrelid) \
            FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
            WHERE a.attrelid = to_regclass($1) AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum";
        const REST: &str = "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = to_regclass($1)";
        const INDEXES: &str = "SELECT pg_get_indexdef(indexrelid) FROM pg_index WHERE indrelid = to_regclass($1) AND NOT indisprimary";
        let name = table.split('.').map(|part| format!("\"{}\"", part.replace('"', "\"\""))).collect::<Vec<_>>().join(".");
        let rows = self.client.query(COLUMNS, &[&name]).map_err(error)?;
        if rows.is_empty() {
            return Err(DbError::Refused(format!("No table {table}")));
        }
        let mut lines: Vec<String> = rows
            .iter()
            .map(|r| {
                let (column, kind, not_null, default): (String, String, bool, Option<String>) = (r.get(0), r.get(1), r.get(2), r.get(3));
                let default = default.map(|d| format!(" DEFAULT {d}")).unwrap_or_default();
                format!("  \"{column}\" {kind}{}{default}", if not_null { " NOT NULL" } else { "" })
            })
            .collect();
        let constraints = self.client.query(REST, &[&name]).map_err(error)?;
        lines.extend(constraints.iter().map(|r| format!("  {}", r.get::<_, String>(0))));
        let mut text = format!("CREATE TABLE {name} (\n{}\n);", lines.join(",\n"));
        for row in self.client.query(INDEXES, &[&name]).map_err(error)? {
            text.push_str(&format!("\n{};", row.get::<_, String>(0)));
        }
        Ok(text)
    }

    fn set_manual(&mut self, manual: bool) -> DbResult<()> {
        self.batch(if manual { "BEGIN" } else { "COMMIT" })?;
        self.manual = manual;
        Ok(())
    }

    fn commit(&mut self) -> DbResult<()> {
        self.batch(if self.manual { "COMMIT; BEGIN" } else { "COMMIT" })
    }

    fn rollback(&mut self) -> DbResult<()> {
        self.batch(if self.manual { "ROLLBACK; BEGIN" } else { "ROLLBACK" })
    }

    fn begin(&mut self) -> DbResult<()> {
        self.batch("BEGIN")
    }

    fn ping(&mut self) -> bool {
        self.client.is_valid(Duration::from_secs(4)).is_ok()
    }

    fn run(&mut self, sql: &str, sink: &mut dyn Sink) -> DbResult<()> {
        self.run_it(sql, sink).map_err(|e| match e {
            DbError::RefusedAt(text, chars) => DbError::RefusedAt(text, sql.char_indices().nth(chars).map_or(sql.len(), |(at, _)| at)),
            other => other,
        })
    }

    fn stopper(&self) -> Arc<dyn Stop> {
        self.stopper.clone()
    }

    fn encrypted(&self) -> bool {
        self.connector.is_some()
    }
}

impl Postgres {
    fn run_it(&mut self, sql: &str, sink: &mut dyn Sink) -> DbResult<()> {
        if !copies(sql) {
            return self.simple(sql, sink);
        }
        let prepared = self.client.prepare(sql).map_err(error)?;
        let columns = prepared.columns();
        // `WITH … DELETE` without `RETURNING` has nothing for `COPY` to carry.
        if columns.is_empty() {
            return self.simple(sql, sink);
        }
        let kinds: Vec<Kind> = columns.iter().map(|c| kind_of(c.type_())).collect();
        let places: Vec<(Option<u32>, Option<i16>)> = columns.iter().map(|c| (c.table_oid().filter(|o| *o != 0), c.column_id().filter(|i| *i > 0))).collect();
        let names: Vec<(String, Kind)> = columns.iter().zip(&kinds).map(|(c, kind)| (c.name().to_string(), *kind)).collect();
        let sources = self.sources(&places);
        let info: Vec<ColumnInfo> = names.into_iter().zip(sources).map(|((name, kind), source)| ColumnInfo { name, kind, source }).collect();
        if sink.columns(info) == Flow::Stop {
            sink.done(0);
            return Ok(());
        }
        let outcome = self.copy(sql, &kinds, sink);
        match outcome {
            Ok(count) => {
                sink.done(count);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_s_text_comes_back_as_written() {
        assert_eq!(unescaped("\\N"), None);
        assert_eq!(unescaped("a\\tb\\nc\\\\d").as_deref(), Some("a\tb\nc\\d"));
        assert_eq!(unescaped("").as_deref(), Some(""));
    }

    #[test]
    fn bytes_and_exact_numbers_keep_their_type() {
        assert_eq!(cell(Kind::Binary, Some("\\xab01".into())), Cell::Bytes(vec![0xab, 1].into()));
        assert_eq!(cell(Kind::Decimal, Some("12345678901234567890.10".into())), Cell::Decimal("12345678901234567890.10".into()));
        assert_eq!(cell(Kind::Text, None), Cell::Null);
    }

    #[test]
    fn only_plain_reads_go_through_copy() {
        assert!(copies("select 1"));
        assert!(copies("  WITH x AS (select 1) select * from x"));
        assert!(!copies("show server_version"));
        assert!(copies("insert into t values (1) returning id"), "a long RETURNING streams too");
        assert!(!copies("insert into t values (1)"));
        assert!(!copies("select 1 into t2"));
    }
}
