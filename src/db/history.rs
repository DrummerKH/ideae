//! What was run, by whom it was connected to: kept next to the connections,
//! readable by its owner only, as a query can hold what a query holds.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Serialize};

const KEPT: usize = 1000;
/// A longer statement (a dump pasted in) is not worth keeping.
const LONGEST: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub at: u64,
    pub connection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    pub sql: String,
}

static ENTRIES: Mutex<Option<Vec<Entry>>> = Mutex::new(None);
static UNSAVED: AtomicBool = AtomicBool::new(false);
static WRITING: Mutex<()> = Mutex::new(());

fn path() -> PathBuf {
    crate::state::state_dir().join("sql_history.json")
}

fn load(path: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
    let before = entries.len();
    entries.retain(|e| !holds_secret(&e.sql));
    if entries.len() != before {
        UNSAVED.store(true, Ordering::Release);
    }
    entries
}

/// A statement that sets a password: it runs, and is not written down.
pub fn holds_secret(sql: &str) -> bool {
    let lower = sql.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let first = words.first().copied().unwrap_or("");
    let has = |text: &str| lower.contains(text);
    has("identified by")
        || has("identified with")
        || has("set password")
        || has("requirepass")
        || has("masterauth")
        || has("master_password")
        || has("source_password")
        || has("acl setuser")
        || first == "auth"
        || (matches!(first, "create" | "alter") && matches!(words.get(1).copied(), Some("user" | "role" | "server" | "subscription")) && (has("password") || has("conninfo")))
}

fn added(entries: &mut Vec<Entry>, entry: Entry) {
    // The same statement again moves up rather than filling the list.
    entries.retain(|e| !(e.sql == entry.sql && e.connection == entry.connection && e.database == entry.database));
    entries.insert(0, entry);
    entries.truncate(KEPT);
}

/// Off the UI thread: the first call reads the file.
pub fn add(connection: String, database: Option<String>, statements: Vec<String>) {
    std::thread::spawn(move || {
        {
            let mut entries = ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
            let entries = entries.get_or_insert_with(|| load(&path()));
            for sql in statements.into_iter().filter(|s| s.len() <= LONGEST && !holds_secret(s)) {
                added(entries, Entry { at: crate::state::now(), connection: connection.clone(), database: database.clone(), sql });
            }
        }
        UNSAVED.store(true, Ordering::Release);
        flush();
    });
}

/// The latest first; reads the file the first time: not for the UI thread.
pub fn list() -> Vec<Entry> {
    ENTRIES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| load(&path())).clone()
}

fn change(apply: impl FnOnce(&mut Vec<Entry>) + Send + 'static) {
    std::thread::spawn(move || {
        {
            let mut entries = ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
            apply(entries.get_or_insert_with(|| load(&path())));
        }
        UNSAVED.store(true, Ordering::Release);
        flush();
    });
}

/// Forgets one statement.
pub fn remove(entry: Entry) {
    change(move |entries| entries.retain(|e| !(e.sql == entry.sql && e.connection == entry.connection && e.database == entry.database)));
}

/// Forgets what ran on this connection, or, with none, everything.
pub fn clear(connection: Option<String>) {
    change(move |entries| match &connection {
        Some(id) => entries.retain(|e| e.connection != *id),
        None => entries.clear(),
    });
}

pub fn flush() {
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if !UNSAVED.swap(false, Ordering::AcqRel) {
        return;
    }
    let Some(entries) = ENTRIES.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
    let Ok(json) = serde_json::to_vec(&entries) else { return };
    if let Err(e) = super::config::write_private(&path(), &json) {
        eprintln!("sql history: {e}");
        UNSAVED.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sql: &str) -> Entry {
        Entry { at: 1, connection: "c".into(), database: None, sql: sql.into() }
    }

    #[test]
    fn a_statement_that_sets_a_password_is_not_written_down() {
        for sql in [
            "CREATE USER 'a'@'%' IDENTIFIED BY 'secret'",
            "ALTER USER a IDENTIFIED WITH caching_sha2_password BY 'x'",
            "SET PASSWORD FOR a = 'x'",
            "create role app login password 'x'",
            "ALTER ROLE app PASSWORD 'x'",
            "AUTH user secret",
            "CONFIG SET requirepass secret",
            "ACL SETUSER a on >secret",
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = 'x'",
        ] {
            assert!(holds_secret(sql), "{sql}");
        }
        assert!(!holds_secret("SELECT password_changed_at FROM users"));
        assert!(!holds_secret("UPDATE users SET name = 'a'"));
    }

    #[test]
    fn the_same_statement_moves_up_and_the_list_has_a_bound() {
        let mut list = Vec::new();
        added(&mut list, entry("a"));
        added(&mut list, entry("b"));
        added(&mut list, entry("a"));
        assert_eq!(list.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        for i in 0..KEPT + 5 {
            added(&mut list, entry(&i.to_string()));
        }
        assert_eq!(list.len(), KEPT);
    }
}
