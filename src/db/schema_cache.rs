//! What a database held when last read, on disk: a large schema shows at
//! once, and is read again only when a quick look says it changed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::schema::Schema;

/// Schemas kept; the ones used longest ago go first.
const KEPT: usize = 40;

#[derive(Serialize, Deserialize)]
struct Cached {
    stamp: String,
    schema: Schema,
}

fn dir() -> PathBuf {
    crate::state::state_dir().join("schema_cache")
}

fn file_in(dir: &Path, connection: &str, database: &str) -> PathBuf {
    let hash = Sha256::digest(format!("{connection}\0{database}").as_bytes());
    let name: String = hash.iter().take(16).map(|b| format!("{b:02x}")).collect();
    connection_dir(dir, connection).join(format!("{name}.json"))
}

/// A folder for each connection: removing one takes all of its databases along.
fn connection_dir(dir: &Path, connection: &str) -> PathBuf {
    let hash = Sha256::digest(connection.as_bytes());
    dir.join(hash.iter().take(12).map(|b| format!("{b:02x}")).collect::<String>())
}

fn read_in(dir: &Path, connection: &str, database: &str) -> Option<(String, Schema)> {
    let bytes = std::fs::read(file_in(dir, connection, database)).ok()?;
    let cached: Cached = serde_json::from_slice(&bytes).ok()?;
    Some((cached.stamp, cached.schema))
}

fn write_in(dir: &Path, connection: &str, database: &str, stamp: &str, schema: &Schema) {
    let Ok(json) = serde_json::to_vec(&Cached { stamp: stamp.to_string(), schema: schema.clone() }) else { return };
    if super::config::write_private(&file_in(dir, connection, database), &json).is_err() {
        return;
    }
    let Ok(connections) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = connections
        .flatten()
        .filter_map(|c| std::fs::read_dir(c.path()).ok())
        .flat_map(|entries| entries.flatten())
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in files.into_iter().skip(KEPT) {
        let _ = std::fs::remove_file(old);
    }
}

/// The tests' own schemas stay out of the user's data.
pub fn read(connection: &str, database: &str) -> Option<(String, Schema)> {
    if cfg!(test) { None } else { read_in(&dir(), connection, database) }
}

pub fn write(connection: &str, database: &str, stamp: &str, schema: &Schema) {
    if !cfg!(test) {
        write_in(&dir(), connection, database, stamp, schema);
    }
}

/// With its connection, what was read of it goes.
pub fn forget(connection: &str) {
    let _ = std::fs::remove_dir_all(connection_dir(&dir(), connection));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_schema_comes_back_with_its_stamp_and_others_do_not_mix() {
        let dir = std::env::temp_dir().join(format!("ideae-schema-cache-{}", crate::db::config::new_id()));
        let row = ("t".to_string(), "BASE TABLE".to_string(), "id".to_string(), "int".to_string(), "NO".to_string(), "PRI".to_string());
        let schema = Schema::from_rows("shop".into(), [row]);
        write_in(&dir, "c1", "shop", "stamp", &schema);
        assert_eq!(read_in(&dir, "c1", "shop"), Some(("stamp".to_string(), schema)));
        assert_eq!(read_in(&dir, "c1", "other"), None);
        assert_eq!(read_in(&dir, "c2", "shop"), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
