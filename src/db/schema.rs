//! What a database holds, and what can be typed at a place in a statement:
//! tables after `FROM`, a table's columns after `name.` (its alias too), the
//! columns of the tables the statement reads, and SQL's keywords.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    /// As the server writes it: `bigint unsigned`, `varchar(255)`.
    pub kind: String,
    pub nullable: bool,
    /// `PRI`, `UNI`, `MUL` or nothing.
    pub key: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableMeta {
    pub name: String,
    pub view: bool,
    pub columns: Vec<ColumnMeta>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub database: String,
    pub tables: Vec<TableMeta>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
    #[serde(default)]
    pub indexes: Vec<IndexMeta>,
    #[serde(default)]
    pub routines: Vec<RoutineMeta>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMeta {
    pub table: String,
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// A stored procedure or function.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineMeta {
    pub name: String,
    pub kind: String,
}

/// `table.column` refers to `to_table.to_column`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub table: String,
    pub column: String,
    pub to_table: String,
    pub to_column: String,
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&TableMeta> {
        self.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name))
    }

    /// From rows of (table, table type, column, column type, nullable, key), in order.
    pub fn from_rows(database: String, rows: impl IntoIterator<Item = (String, String, String, String, String, String)>) -> Schema {
        let mut tables: Vec<TableMeta> = Vec::new();
        for (table, kind, column, column_kind, nullable, key) in rows {
            if tables.last().is_none_or(|t| t.name != table) {
                tables.push(TableMeta { name: table, view: kind.eq_ignore_ascii_case("VIEW"), columns: Vec::new() });
            }
            if let Some(last) = tables.last_mut() {
                last.columns.push(ColumnMeta { name: column, kind: column_kind, nullable: nullable.eq_ignore_ascii_case("YES"), key });
            }
        }
        Schema { database, tables, ..Default::default() }
    }
}

pub const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "NULL", "IS", "IN", "LIKE", "BETWEEN", "EXISTS", "CASE", "WHEN", "THEN", "ELSE", "END", "AS", "DISTINCT",
    "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "CROSS", "ON", "USING", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "OFFSET", "ASC", "DESC", "UNION", "ALL",
    "INSERT INTO", "VALUES", "UPDATE", "SET", "DELETE FROM", "CREATE TABLE", "ALTER TABLE", "DROP TABLE", "TRUNCATE TABLE", "CREATE INDEX", "SHOW", "DESCRIBE",
    "EXPLAIN", "WITH", "BEGIN", "COMMIT", "ROLLBACK", "COUNT", "SUM", "AVG", "MIN", "MAX", "COALESCE", "IFNULL", "CONCAT", "NOW", "CAST",
];

/// Functions most servers have, offered as they are typed.
pub const FUNCTIONS: &[&str] = &[
    "ABS", "CEIL", "FLOOR", "ROUND", "MOD", "LENGTH", "CHAR_LENGTH", "LOWER", "UPPER", "TRIM", "LTRIM", "RTRIM", "SUBSTRING", "REPLACE", "POSITION", "LPAD", "RPAD",
    "NULLIF", "GREATEST", "LEAST", "CURRENT_DATE", "CURRENT_TIMESTAMP", "EXTRACT", "DATE", "YEAR", "MONTH", "DAY", "HOUR", "DATE_ADD", "DATE_SUB", "DATEDIFF", "DATE_FORMAT",
    "DATE_TRUNC", "TO_CHAR", "STRING_AGG", "GROUP_CONCAT", "JSON_EXTRACT", "JSON_OBJECT", "JSON_ARRAY", "JSONB_BUILD_OBJECT", "ROW_NUMBER", "RANK", "DENSE_RANK", "LAG", "LEAD",
];

/// One thing that can be typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub text: String,
    pub detail: String,
    pub kind: SuggestionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SuggestionKind {
    /// A table with the condition that joins it, from a foreign key.
    Join,
    Column,
    Table,
    Function,
    Keyword,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The words of a statement outside strings and comments, a quoted name as its name.
fn words(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' {
            i += 1;
            while i < chars.len() && chars[i] != c {
                i += if chars[i] == '\\' { 2 } else { 1 };
            }
            i += 1;
        } else if c == '`' {
            let start = i + 1;
            i = start;
            while i < chars.len() && chars[i] != '`' {
                i += 1;
            }
            out.push(chars[start..i.min(chars.len())].iter().collect());
            i += 1;
        } else if c == '#' || (c == '-' && chars.get(i + 1) == Some(&'-')) {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if is_word(c) {
            let start = i;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        } else {
            if matches!(c, '.' | ',' | '(' | ')') {
                out.push(c.to_string());
            }
            i += 1;
        }
    }
    out
}

/// What the name at a place in a statement is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameAt {
    /// An alias: where the statement declares it.
    Alias(std::ops::Range<usize>),
    Table(String),
    Column { table: String, column: String },
}

/// The names of a statement outside strings and comments, with their bytes.
fn name_tokens(sql: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let bytes = sql.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\'' || b == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b {
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
        } else if b == b'`' {
            let start = i + 1;
            i = start;
            while i < bytes.len() && bytes[i] != b'`' {
                i += 1;
            }
            out.push((start..i.min(bytes.len()), sql[start..i.min(bytes.len())].to_string()));
            i += 1;
        } else if b == b'#' || bytes[i..].starts_with(b"--") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i..].starts_with(b"/*") {
            i += 2;
            while i + 1 < bytes.len() && !bytes[i..].starts_with(b"*/") {
                i += 1;
            }
            i += 2;
        } else if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80 {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$' || bytes[i] >= 0x80) {
                i += 1;
            }
            out.push((start..i, sql[start..i].to_string()));
        } else {
            i += 1;
        }
    }
    out
}

/// Tells the name at byte `at`: an alias needs no database, the rest the schema.
pub fn name_at(schema: Option<&Schema>, sql: &str, at: usize) -> Option<NameAt> {
    let tokens = name_tokens(sql);
    let ix = tokens.iter().position(|(place, _)| place.start <= at && at <= place.end)?;
    let (place, word) = &tokens[ix];
    let known = tables_in(sql);
    let dotted = ix > 0 && sql[..place.start].trim_end_matches('`').ends_with('.');
    let qualifier = dotted.then(|| tokens[ix - 1].1.clone());
    if qualifier.is_none()
        && let Some(table) = known.get(&word.to_ascii_lowercase()).filter(|t| !t.eq_ignore_ascii_case(word))
    {
        // The table's name, an optional AS, and this name after them.
        let declared = tokens.windows(3).find_map(|w| (w[0].1.eq_ignore_ascii_case(table) && w[1].1.eq_ignore_ascii_case("as") && w[2].1.eq_ignore_ascii_case(word)).then(|| w[2].0.clone()));
        let declared = declared.or_else(|| tokens.windows(2).find_map(|w| (w[0].1.eq_ignore_ascii_case(table) && w[1].1.eq_ignore_ascii_case(word)).then(|| w[1].0.clone())));
        return declared.map(NameAt::Alias);
    }
    let schema = schema?;
    if let Some(qualifier) = qualifier {
        let table = known.get(&qualifier.to_ascii_lowercase()).cloned().unwrap_or(qualifier.clone());
        if let Some(table) = schema.table(&table)
            && let Some(column) = table.columns.iter().find(|c| c.name.eq_ignore_ascii_case(word))
        {
            return Some(NameAt::Column { table: table.name.clone(), column: column.name.clone() });
        }
        if qualifier.eq_ignore_ascii_case(&schema.database) {
            return schema.table(word).map(|t| NameAt::Table(t.name.clone()));
        }
        return None;
    }
    if let Some(table) = schema.table(word) {
        return Some(NameAt::Table(table.name.clone()));
    }
    let mut read: Vec<&String> = known.values().collect();
    read.sort();
    read.dedup();
    read.into_iter().filter_map(|t| schema.table(t)).find_map(|t| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(word)).map(|c| NameAt::Column { table: t.name.clone(), column: c.name.clone() }))
}

const AFTER_TABLE: &[&str] = &["where", "on", "using", "group", "order", "having", "limit", "join", "left", "right", "inner", "outer", "cross", "set", "union", "values", "as", "for"];

/// The tables a statement names, each with what it is called there.
pub fn tables_in(sql: &str) -> HashMap<String, String> {
    let words = words(sql);
    let mut found = HashMap::new();
    for (i, word) in words.iter().enumerate() {
        if !matches!(word.to_ascii_lowercase().as_str(), "from" | "join" | "update" | "into") {
            continue;
        }
        let mut at = i + 1;
        loop {
            let Some(mut table) = words.get(at).cloned() else { break };
            if table == "(" {
                break;
            }
            at += 1;
            // `db.table`: the table.
            if words.get(at).is_some_and(|w| w == ".") {
                let Some(name) = words.get(at + 1) else { break };
                table = name.clone();
                at += 2;
            }
            found.insert(table.to_ascii_lowercase(), table.clone());
            if words.get(at).is_some_and(|w| w.eq_ignore_ascii_case("as")) {
                at += 1;
            }
            if let Some(alias) = words.get(at).filter(|w| w.chars().all(is_word) && !AFTER_TABLE.contains(&w.to_ascii_lowercase().as_str())) {
                found.insert(alias.to_ascii_lowercase(), table.clone());
                at += 1;
            }
            if words.get(at).is_some_and(|w| w == ",") {
                at += 1;
            } else {
                break;
            }
        }
    }
    found
}

/// What fits at the end of `before`, the statement up to the cursor, in a
/// statement `whole`; the word being typed is left for the menu to filter by.
pub fn suggest(schema: &Schema, before: &str, whole: &str) -> Vec<Suggestion> {
    let typed_at = before.rfind(|c: char| !is_word(c)).map_or(0, |i| i + before[i..].chars().next().map_or(1, char::len_utf8));
    let (head, typed) = before.split_at(typed_at);
    let typed = typed.to_lowercase();
    let keep = |text: &str| text.to_lowercase().starts_with(&typed);
    let column = |c: &ColumnMeta, table: &str| Suggestion { text: c.name.clone(), detail: format!("{} · {table}", c.kind), kind: SuggestionKind::Column };
    let known = tables_in(whole);

    // `name.`: that table's columns.
    if let Some(qualifier) = head.strip_suffix('.') {
        let name = qualifier.rsplit(|c: char| !is_word(c) && c != '`').next().unwrap_or("").trim_matches('`').to_ascii_lowercase();
        let table = known.get(&name).map_or(name.as_str(), String::as_str);
        if let Some(table) = schema.table(table) {
            return table.columns.iter().filter(|c| keep(&c.name)).map(|c| column(c, &table.name)).collect();
        }
        // `database.`: its tables, when it is this one.
        if name.eq_ignore_ascii_case(&schema.database) {
            return schema.tables.iter().filter(|t| keep(&t.name)).map(table_suggestion).collect();
        }
        return Vec::new();
    }

    let previous = words(head).last().map(|w| w.to_ascii_lowercase()).unwrap_or_default();
    if previous == "join" {
        let mut out = joins(schema, head, &known, &keep);
        out.extend(schema.tables.iter().filter(|t| keep(&t.name)).map(table_suggestion));
        return out;
    }
    if matches!(previous.as_str(), "from" | "update" | "into" | "table" | "describe" | "desc") {
        return schema.tables.iter().filter(|t| keep(&t.name)).map(table_suggestion).collect();
    }

    let mut out: Vec<Suggestion> = Vec::new();
    let mut read: Vec<&String> = known.values().collect();
    read.sort();
    read.dedup();
    for table in read {
        if let Some(table) = schema.table(table) {
            out.extend(table.columns.iter().filter(|c| keep(&c.name)).map(|c| column(c, &table.name)));
        }
    }
    out.extend(schema.tables.iter().filter(|t| keep(&t.name)).map(table_suggestion));
    if !typed.is_empty() {
        out.extend(FUNCTIONS.iter().filter(|k| keep(k)).map(|k| Suggestion { text: (*k).to_string(), detail: "function".into(), kind: SuggestionKind::Function }));
        out.extend(KEYWORDS.iter().filter(|k| keep(k)).map(|k| Suggestion { text: (*k).to_string(), detail: "keyword".into(), kind: SuggestionKind::Keyword }));
    }
    out
}

/// After `JOIN`: the tables a foreign key ties to those already read, with
/// the whole condition, written with the names the statement uses for them.
fn joins(schema: &Schema, head: &str, known: &HashMap<String, String>, keep: &dyn Fn(&str) -> bool) -> Vec<Suggestion> {
    let before = tables_in(head);
    // What a table is called here: its alias, when it has one.
    let called = |table: &str| -> Option<String> {
        let names: Vec<&String> = before.iter().filter(|(_, t)| t.eq_ignore_ascii_case(table)).map(|(name, _)| name).collect();
        names.iter().find(|n| !n.eq_ignore_ascii_case(table)).or(names.first()).map(|n| (*n).clone())
    };
    let _ = known;
    let mut out = Vec::new();
    for key in &schema.foreign_keys {
        // Both ways: orders to users, and users to their orders.
        let ways = [(&key.to_table, &key.to_column, &key.table, &key.column), (&key.table, &key.column, &key.to_table, &key.to_column)];
        for (new, new_column, there, there_column) in ways {
            let Some(name) = called(there) else { continue };
            if !keep(new) || called(new).is_some() {
                continue;
            }
            let text = format!("{new} ON {new}.{new_column} = {name}.{there_column}");
            if !out.iter().any(|s: &Suggestion| s.text == text) {
                out.push(Suggestion { text, detail: "by foreign key".into(), kind: SuggestionKind::Join });
            }
        }
    }
    out
}

fn table_suggestion(table: &TableMeta) -> Suggestion {
    Suggestion {
        text: table.name.clone(),
        detail: format!("{} · {} columns", if table.view { "view" } else { "table" }, table.columns.len()),
        kind: SuggestionKind::Table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        let row = |t: &str, c: &str| (t.to_string(), "BASE TABLE".to_string(), c.to_string(), "int".to_string(), "NO".to_string(), String::new());
        Schema::from_rows("shop".into(), [row("orders", "id"), row("orders", "user_id"), row("users", "id"), row("users", "email")])
    }

    fn texts(before: &str) -> Vec<String> {
        suggest(&schema(), before, before).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn rows_of_the_catalog_make_tables_with_their_columns() {
        let s = schema();
        assert_eq!(s.tables.len(), 2);
        assert_eq!(s.table("USERS").unwrap().columns[1].name, "email");
    }

    #[test]
    fn after_from_come_tables() {
        assert_eq!(texts("select * from "), ["orders", "users"]);
        assert_eq!(texts("select * from us"), ["users"]);
    }

    #[test]
    fn after_a_name_and_a_dot_come_its_columns_by_alias_too() {
        let whole = "select u.e from users u";
        let found: Vec<String> = suggest(&schema(), "select u.e", whole).into_iter().map(|s| s.text).collect();
        assert_eq!(found, ["email"]);
        let found: Vec<String> = suggest(&schema(), "select o.", "select o. from `orders` AS o").into_iter().map(|s| s.text).collect();
        assert_eq!(found, ["id", "user_id"]);
    }

    #[test]
    fn after_join_a_foreign_key_gives_the_whole_condition() {
        let mut s = schema();
        s.foreign_keys.push(ForeignKey { table: "orders".into(), column: "user_id".into(), to_table: "users".into(), to_column: "id".into() });
        let before = "select * from orders o join ";
        let found = suggest(&s, before, before);
        assert_eq!(found[0].text, "users ON users.id = o.user_id");
        assert_eq!(found[0].kind, SuggestionKind::Join);
        let before = "select * from users join ";
        assert_eq!(suggest(&s, before, before)[0].text, "orders ON orders.user_id = users.id", "and the other way");
        let before = "select * from orders o join users u join ";
        assert!(suggest(&s, before, before).iter().all(|f| f.kind != SuggestionKind::Join), "not one already there");
    }

    #[test]
    fn a_name_is_told_by_where_it_stands() {
        let s = schema();
        let sql = "select o.user_id, email from orders AS o join users u on u.id = o.user_id where note = 'orders'";
        let at = |word: &str| sql.find(word).unwrap() + 1;
        assert_eq!(name_at(Some(&s), sql, at("o.user_id")), Some(NameAt::Alias(sql.find("o join").unwrap()..sql.find("o join").unwrap() + 1)));
        assert_eq!(name_at(None, sql, at("u.id")), Some(NameAt::Alias(sql.find("u on").unwrap()..sql.find("u on").unwrap() + 1)), "an alias needs no connection");
        assert_eq!(name_at(Some(&s), sql, at("user_id")), Some(NameAt::Column { table: "orders".into(), column: "user_id".into() }));
        assert_eq!(name_at(Some(&s), sql, at("email")), Some(NameAt::Column { table: "users".into(), column: "email".into() }));
        assert_eq!(name_at(Some(&s), sql, at("users")), Some(NameAt::Table("users".into())));
        assert_eq!(name_at(Some(&s), sql, sql.rfind("orders").unwrap() + 1), None, "inside a string");
        assert_eq!(name_at(None, sql, at("users")), None);
    }

    #[test]
    fn functions_are_offered_as_they_are_typed() {
        let found = suggest(&schema(), "select coal", "select coal");
        assert!(found.iter().any(|f| f.text == "COALESCE"));
        let found = suggest(&schema(), "select row_n", "select row_n");
        assert_eq!(found[0].kind, SuggestionKind::Function);
    }

    #[test]
    fn columns_of_the_tables_read_come_before_keywords() {
        let whole = "select * from users where em";
        let found = suggest(&schema(), whole, whole);
        assert_eq!(found[0].text, "email");
        assert_eq!(found[0].kind, SuggestionKind::Column);
    }

    #[test]
    fn the_tables_of_a_statement_are_found_past_joins_and_commas() {
        let found = tables_in("select * from a x, b as y join shop.c on 1 where 'from z' = 1");
        assert_eq!(found.get("x").map(String::as_str), Some("a"));
        assert_eq!(found.get("y").map(String::as_str), Some("b"));
        assert_eq!(found.get("c").map(String::as_str), Some("c"));
        assert!(!found.contains_key("z"));
    }
}
