//! Redis: a `.redis` file holds a command a line, its answer a small table.
//! Keys stand where a SQL database has tables, read with `SCAN`, never `KEYS`,
//! which holds the whole server up.

use std::{sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use redis::Value;

use super::{
    backend::{Backend, ColumnInfo, DbError, DbResult, Flow, Sink, Stop, address},
    config::{Connection, TlsMode},
    schema::{ColumnMeta, Schema, TableMeta},
    tunnel::Tunnel,
    value::{Cell, Kind, Row},
};

const CONNECT_WITHIN: Duration = Duration::from_secs(15);
/// Keys listed in the Schema tab.
const KEYS_LISTED: usize = 2000;

pub struct Redis {
    conn: redis::Connection,
    stopper: Arc<Unblocker>,
    encrypted: bool,
}

/// What can be stopped of Redis is a command that waits (`BLPOP`, `XREAD`
/// and the like), from a second connection; one that works is over at once.
struct Unblocker {
    client: redis::Client,
    id: Option<i64>,
}

impl Stop for Unblocker {
    fn stop(&self) -> Result<()> {
        let Some(id) = self.id else { return Ok(()) };
        let mut conn = self.client.get_connection_with_timeout(CONNECT_WITHIN)?;
        redis::cmd("CLIENT").arg("UNBLOCK").arg(id).arg("ERROR").query::<i64>(&mut conn)?;
        Ok(())
    }
}

fn error(e: redis::RedisError) -> DbError {
    if e.to_string().contains("UNBLOCKED") {
        return DbError::Interrupted;
    }
    if e.is_connection_dropped() || e.is_io_error() || e.is_timeout() { DbError::Gone(e.to_string()) } else { DbError::Refused(e.to_string()) }
}

/// A line's words, as a shell or `redis-cli` parts them.
pub fn words(line: &str) -> Result<Vec<Vec<u8>>, String> {
    let (mut out, mut word, mut chars) = (Vec::new(), None::<Vec<u8>>, line.chars().peekable());
    let push = |word: &mut Vec<u8>, c: char| word.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => out.extend(word.take()),
            '\'' => {
                let current = word.get_or_insert_with(Vec::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some('\\') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            current.push(b'\'');
                        }
                        Some(c) => push(current, c),
                        None => return Err("A quote is not closed".into()),
                    }
                }
            }
            '"' => {
                let current = word.get_or_insert_with(Vec::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => current.push(b'\n'),
                            Some('r') => current.push(b'\r'),
                            Some('t') => current.push(b'\t'),
                            Some('x') => {
                                let hex: String = chars.by_ref().take(2).collect();
                                current.push(u8::from_str_radix(&hex, 16).map_err(|_| "A \\x escape needs two hex digits".to_string())?);
                            }
                            Some(c) => push(current, c),
                            None => return Err("A quote is not closed".into()),
                        },
                        Some(c) => push(current, c),
                        None => return Err("A quote is not closed".into()),
                    }
                }
            }
            c => push(word.get_or_insert_with(Vec::new), c),
        }
    }
    out.extend(word);
    Ok(out)
}

fn cell(value: &Value) -> Cell {
    match value {
        Value::Nil => Cell::Null,
        Value::Int(n) => Cell::Int(*n),
        Value::BulkString(bytes) => Kind::Text.cell(mysql::Value::Bytes(bytes.clone())),
        Value::SimpleString(text) => Cell::Text(text.as_str().into()),
        Value::Okay => Cell::Text("OK".into()),
        Value::Double(n) => Cell::Float(n.to_string().into()),
        Value::Boolean(b) => Cell::Int(i64::from(*b)),
        Value::BigNumber(n) => Cell::Decimal(String::from_utf8_lossy(n).as_ref().into()),
        Value::VerbatimString { text, .. } => Cell::Text(text.as_str().into()),
        other => Cell::Text(format!("{other:?}").into()),
    }
}

/// An answer as columns and rows: a list has an index, a map its fields.
fn table(value: &Value, paired: Option<[&'static str; 2]>) -> (Vec<&'static str>, Vec<Vec<Cell>>) {
    match value {
        // RESP2 has no map: these commands answer with a flat list of pairs.
        Value::Array(items) if paired.is_some() && items.len() % 2 == 0 => {
            let names = paired.unwrap_or(["field", "value"]);
            (names.to_vec(), items.chunks_exact(2).map(|pair| vec![cell(&pair[0]), cell(&pair[1])]).collect())
        }
        Value::Array(items) | Value::Set(items) => (vec!["#", "value"], items.iter().enumerate().map(|(i, v)| vec![Cell::Int(i as i64 + 1), cell(v)]).collect()),
        Value::Map(pairs) => (vec!["field", "value"], pairs.iter().map(|(k, v)| vec![cell(k), cell(v)]).collect()),
        other => (vec!["value"], vec![vec![cell(other)]]),
    }
}

impl Redis {
    pub fn connect(connection: &Connection, tunnel: Option<&Tunnel>, password: Option<String>, database: Option<String>) -> Result<Self> {
        let (host, port) = address(connection, tunnel);
        let mut settings = redis::RedisConnectionInfo::default().set_db(database.as_deref().and_then(|d| d.parse().ok()).unwrap_or(0));
        if !connection.user.is_empty() {
            settings = settings.set_username(&connection.user);
        }
        if let Some(password) = password.filter(|p| !p.is_empty()) {
            settings = settings.set_password(password);
        }
        let addr = match connection.tls.mode {
            TlsMode::Off => redis::ConnectionAddr::Tcp(host, port),
            // Through a tunnel its name would be compared with this machine's.
            TlsMode::Verify if tunnel.is_some() => anyhow::bail!("Redis: Verify cannot check a certificate's name through a tunnel. Use Require: the tunnel is what is trusted"),
            mode => redis::ConnectionAddr::TcpTls { host, port, insecure: mode != TlsMode::Verify, tls_params: None },
        };
        let info = redis::IntoConnectionInfo::into_connection_info(addr).map_err(|e| anyhow!("Redis: {e}"))?.set_redis_settings(settings);
        let client = match connection.tls.ca.as_ref().filter(|_| connection.tls.mode == TlsMode::Verify) {
            Some(ca) => {
                let root_cert = Some(std::fs::read(ca).map_err(|e| anyhow!("The CA file {}: {e}", ca.display()))?);
                redis::Client::build_with_tls(info, redis::TlsCertificates { client_tls: None, root_cert })
            }
            None => redis::Client::open(info),
        }
        .map_err(|e| anyhow!("Redis: {e}"))?;
        let mut conn = client.get_connection_with_timeout(CONNECT_WITHIN).map_err(|e| anyhow!("Redis: {e}"))?;
        // Older servers have no client ids: their waiting commands cannot be stopped.
        let id = redis::cmd("CLIENT").arg("ID").query::<i64>(&mut conn).ok();
        let stopper = Arc::new(Unblocker { client, id });
        Ok(Redis { conn, stopper, encrypted: connection.tls.mode != TlsMode::Off })
    }
}

impl Backend for Redis {
    fn version(&mut self) -> String {
        let info: String = redis::cmd("INFO").arg("server").query(&mut self.conn).unwrap_or_default();
        info.lines().find_map(|l| l.strip_prefix("redis_version:")).unwrap_or_default().trim().to_string()
    }

    fn databases(&mut self) -> DbResult<Vec<String>> {
        // `CONFIG` is often not allowed: then the default sixteen.
        let count = redis::cmd("CONFIG").arg("GET").arg("databases").query::<Vec<String>>(&mut self.conn).ok().and_then(|v| v.get(1).and_then(|n| n.parse::<u32>().ok())).unwrap_or(16);
        Ok((0..count).map(|n| n.to_string()).collect())
    }

    fn use_database(&mut self, name: &str) -> DbResult<()> {
        redis::cmd("SELECT").arg(name).query::<()>(&mut self.conn).map_err(error)
    }

    fn schema(&mut self, database: &str) -> DbResult<Schema> {
        let mut names: Vec<String> = Vec::new();
        let mut cursor: u64 = 0;
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN").arg(cursor).arg("COUNT").arg(500).query(&mut self.conn).map_err(error)?;
            names.extend(batch);
            cursor = next;
            if cursor == 0 || names.len() >= KEYS_LISTED {
                break;
            }
        }
        names.sort();
        names.dedup();
        names.truncate(KEYS_LISTED);
        let mut tables = Vec::with_capacity(names.len());
        for name in names {
            let kind: String = redis::cmd("TYPE").arg(&name).query(&mut self.conn).map_err(error)?;
            tables.push(TableMeta { columns: vec![ColumnMeta { name: "type".into(), kind, nullable: true, key: String::new() }], name, view: false });
        }
        Ok(Schema { database: database.to_string(), tables, ..Default::default() })
    }

    fn schema_stamp(&mut self, _: &str) -> DbResult<Option<String>> {
        Ok(None)
    }

    fn definition(&mut self, key: &str) -> DbResult<String> {
        let kind: String = redis::cmd("TYPE").arg(key).query(&mut self.conn).map_err(error)?;
        let ttl: i64 = redis::cmd("TTL").arg(key).query(&mut self.conn).map_err(error)?;
        let memory: Option<i64> = redis::cmd("MEMORY").arg("USAGE").arg(key).query(&mut self.conn).ok().flatten();
        let ttl = if ttl < 0 { "none".to_string() } else { format!("{ttl} s") };
        let memory = memory.map(|m| format!(" · {m} bytes")).unwrap_or_default();
        Ok(format!("{key}: {kind} · TTL {ttl}{memory}"))
    }

    fn set_manual(&mut self, manual: bool) -> DbResult<()> {
        if manual { Err(DbError::Refused("Redis has no transactions to keep open: use MULTI and EXEC".into())) } else { Ok(()) }
    }

    fn commit(&mut self) -> DbResult<()> {
        Ok(())
    }

    fn rollback(&mut self) -> DbResult<()> {
        Ok(())
    }

    fn begin(&mut self) -> DbResult<()> {
        Err(DbError::Refused("Redis has no transactions to keep open: use MULTI and EXEC".into()))
    }

    fn ping(&mut self) -> bool {
        redis::cmd("PING").query::<String>(&mut self.conn).is_ok()
    }

    fn run(&mut self, line: &str, sink: &mut dyn Sink) -> DbResult<()> {
        let words = words(line).map_err(DbError::Refused)?;
        let Some((name, args)) = words.split_first() else { return Ok(()) };
        // Would take the connection over, and every answer after it.
        let upper = String::from_utf8_lossy(name).to_ascii_uppercase();
        if matches!(upper.as_str(), "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "MONITOR") {
            return Err(DbError::Refused(format!("{upper} streams without end: use redis-cli in a terminal")));
        }
        let mut command = redis::cmd(&upper);
        for arg in args {
            command.arg(arg.as_slice());
        }
        let value: Value = command.query(&mut self.conn).map_err(error)?;
        let with_scores = args.iter().any(|a| a.eq_ignore_ascii_case(b"WITHSCORES"));
        let paired = match upper.as_str() {
            "HGETALL" => Some(["field", "value"]),
            "CONFIG" if args.first().is_some_and(|a| a.eq_ignore_ascii_case(b"GET")) => Some(["name", "value"]),
            "ZRANGE" | "ZREVRANGE" | "ZRANGEBYSCORE" | "ZREVRANGEBYSCORE" if with_scores => Some(["member", "score"]),
            _ => None,
        };
        let (columns, rows) = table(&value, paired);
        let info = columns.iter().map(|name| ColumnInfo { name: (*name).to_string(), kind: Kind::Text, source: None }).collect();
        if sink.columns(info) == Flow::Go {
            for row in rows {
                let bytes = row.iter().map(|c| c.shown().len()).sum();
                if sink.row(row.into_boxed_slice() as Row, bytes) == Flow::Stop {
                    break;
                }
            }
        }
        sink.done(0);
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
    use super::*;

    fn parsed(line: &str) -> Vec<String> {
        words(line).unwrap().into_iter().map(|w| String::from_utf8(w).unwrap()).collect()
    }

    #[test]
    fn a_line_parts_as_redis_cli_parts_it() {
        assert_eq!(parsed("SET key \"a b\\n\" 'it\\'s'"), ["SET", "key", "a b\n", "it's"]);
        assert_eq!(parsed("  GET   k  "), ["GET", "k"]);
        assert_eq!(parsed("SET k \"\""), ["SET", "k", ""]);
        assert!(words("GET \"open").is_err());
    }

    #[test]
    fn an_answer_is_a_table() {
        let (columns, rows) = table(&Value::Array(vec![Value::Int(5), Value::Nil]), None);
        assert_eq!(columns, ["#", "value"]);
        assert_eq!(rows[1][1], Cell::Null);
        assert_eq!(table(&Value::Okay, None).1[0][0], Cell::Text("OK".into()));
        let (columns, rows) = table(&Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]), Some(["field", "value"]));
        assert_eq!((columns, rows.len()), (vec!["field", "value"], 2));
    }
}
