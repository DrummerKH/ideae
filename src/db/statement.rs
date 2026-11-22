//! A SQL text parted into its statements, as the `mysql` client parts it:
//! a delimiter inside a string, a quoted name or a comment does not count,
//! and `DELIMITER` changes it for what follows.

use std::ops::Range;

use super::config::Driver;

/// Where each statement is, without its delimiter and the comments around it.
pub fn split(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut delimiter: &[u8] = b";";
    let mut found = Vec::new();
    let mut current: Option<Range<usize>> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if current.is_none() && let Some((new, next)) = delimiter_command(text, i) {
            delimiter = new;
            i = next;
        } else if bytes[i..].starts_with(delimiter) {
            found.extend(current.take());
            i += delimiter.len();
        } else if b == b'#' || line_comment(bytes, i) {
            i = line_end(bytes, i);
        } else if bytes[i..].starts_with(b"/*") {
            let end = bytes[i + 2..].windows(2).position(|w| w == b"*/").map_or(bytes.len(), |p| i + p + 4);
            // `/*! … */` is run by MySQL, and `/*+ … */` read by it: they are the statement's.
            if current.is_some() || matches!(bytes.get(i + 2), Some(b'!' | b'+')) {
                mark(&mut current, i, end);
            }
            i = end;
        } else if matches!(b, b'\'' | b'"' | b'`') {
            let end = quoted_end(bytes, i);
            mark(&mut current, i, end);
            i = end;
        } else {
            let end = i + 1;
            mark(&mut current, i, end);
            i = end;
        }
    }
    found.extend(current);
    found
}

fn mark(current: &mut Option<Range<usize>>, start: usize, end: usize) {
    current.get_or_insert(start..end).end = end;
}

/// MySQL's `--` is a comment only with a space or a line end after it: `5--1` is a sum.
fn line_comment(bytes: &[u8], i: usize) -> bool {
    bytes[i..].starts_with(b"--") && bytes.get(i + 2).is_none_or(u8::is_ascii_whitespace)
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |p| from + p)
}

/// Just past a quoted string or name: a doubled quote stays inside, and so
/// does one after a backslash, except in a name.
fn quoted_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quote != b'`' => i += 2,
            b if b == quote && bytes.get(i + 1) == Some(&quote) => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// A `DELIMITER x` line at `i`: the new delimiter and where its line ends.
fn delimiter_command(text: &str, i: usize) -> Option<(&[u8], usize)> {
    const WORD: &str = "delimiter";
    let bytes = text.as_bytes();
    let head = text.get(i..i + WORD.len())?;
    if !head.eq_ignore_ascii_case(WORD) || !bytes.get(i + WORD.len()).is_some_and(|b| *b == b' ' || *b == b'\t') {
        return None;
    }
    let end = line_end(bytes, i);
    let new = text[i + WORD.len()..end].split_whitespace().next()?;
    Some((new.as_bytes(), end))
}

/// The statement the cursor is in. Between two, the one it is right after on
/// its line, else the next one, else the last.
pub fn at(text: &str, statements: &[Range<usize>], cursor: usize) -> Option<Range<usize>> {
    if let Some(inside) = statements.iter().find(|s| s.start <= cursor && cursor <= s.end) {
        return Some(inside.clone());
    }
    let before = statements.iter().rev().find(|s| s.end <= cursor);
    if let Some(before) = before
        && !text.get(before.end..cursor).unwrap_or("\n").contains('\n')
    {
        return Some(before.clone());
    }
    statements.iter().find(|s| s.start > cursor).or(before).cloned()
}

/// A statement's words outside strings, comments and parentheses, in lower case.
pub fn top_words(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let (mut out, mut depth, mut i) = (Vec::new(), 0usize, 0);
    while i < bytes.len() {
        let b = bytes[i];
        if matches!(b, b'\'' | b'"' | b'`') {
            i = quoted_end(bytes, i);
        } else if b == b'#' || line_comment(bytes, i) {
            i = line_end(bytes, i);
        } else if bytes[i..].starts_with(b"/*") {
            i = bytes[i + 2..].windows(2).position(|w| w == b"*/").map_or(bytes.len(), |p| i + p + 4);
        } else if b == b'(' {
            depth += 1;
            i += 1;
        } else if b == b')' {
            depth = depth.saturating_sub(1);
            i += 1;
        } else if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] >= 0x80) {
                i += 1;
            }
            if depth == 0 {
                out.push(sql[start..i].to_ascii_lowercase());
            }
        } else {
            i += 1;
        }
    }
    out
}

/// An `UPDATE` or `DELETE` with no `WHERE`: it changes every row.
pub fn changes_every_row(sql: &str) -> bool {
    let words = top_words(sql);
    let verb = match words.first().map(String::as_str) {
        Some("with") => words.iter().any(|w| w == "update" || w == "delete"),
        Some("update" | "delete") => true,
        _ => false,
    };
    verb && !words.iter().any(|w| w == "where")
}

/// Whether it changes data, by its verb: what must not be stopped half way
/// for the table having shown enough of what it returns.
pub fn writes(sql: &str) -> bool {
    let words = top_words(sql);
    let verbs = ["insert", "update", "delete", "replace", "merge", "call"];
    match words.first().map(String::as_str) {
        Some("with") => words.iter().any(|w| verbs.contains(&w.as_str())),
        Some(first) => verbs.contains(&first),
        None => false,
    }
}

/// A `.redis` file: a command on each line that is not empty or a comment.
pub fn lines(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim();
        if !body.is_empty() && !body.starts_with('#') {
            let lead = line.len() - line.trim_start().len();
            found.push(start + lead..start + lead + body.len());
        }
        start += line.len();
    }
    found
}

/// How this server's text parts into what runs.
pub fn split_for(driver: Driver, text: &str) -> Vec<Range<usize>> {
    if driver == Driver::Redis { lines(text) } else { split(text) }
}

/// The `:name` parameters of a statement, each once, in order. Not `::` (a
/// cast elsewhere), `:=` (MySQL's assignment), nor inside strings and comments.
pub fn parameters(sql: &str) -> Vec<String> {
    let mut names = Vec::new();
    for_each_parameter(sql, |_, name| {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    });
    names
}

fn for_each_parameter(sql: &str, mut found: impl FnMut(Range<usize>, &str)) {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if matches!(b, b'\'' | b'"' | b'`') {
            i = quoted_end(bytes, i);
        } else if b == b'#' || line_comment(bytes, i) {
            i = line_end(bytes, i);
        } else if bytes[i..].starts_with(b"/*") {
            i = bytes[i + 2..].windows(2).position(|w| w == b"*/").map_or(bytes.len(), |p| i + p + 4);
        } else if b == b':' && (i == 0 || bytes[i - 1] != b':') && bytes.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') {
            let start = i + 1;
            let mut end = start;
            while bytes.get(end).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
                end += 1;
            }
            found(i..end, &sql[start..end]);
            i = end;
        } else {
            i += 1;
        }
    }
}

/// A typed value as SQL: a number or NULL as it is, anything else a string.
pub fn literal(value: &str, driver: Driver) -> String {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return "NULL".into();
    }
    let digits = trimmed.strip_prefix('-').unwrap_or(trimmed);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, "0"));
    let number = !whole.is_empty() && whole.bytes().all(|b| b.is_ascii_digit()) && !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit());
    if number {
        return trimmed.to_string();
    }
    driver.string(value)
}

/// The statement with each parameter replaced by its value as a literal.
pub fn with_parameters(sql: &str, driver: Driver, value_of: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut from = 0;
    for_each_parameter(sql, |place, name| {
        if let Some(value) = value_of(name) {
            out.push_str(&sql[from..place.start]);
            out.push_str(&literal(&value, driver));
            from = place.end;
        }
    });
    out.push_str(&sql[from..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_of_every_row_is_told_from_one_with_a_condition() {
        assert!(changes_every_row("UPDATE t SET a = 1"));
        assert!(changes_every_row("delete from t"));
        assert!(changes_every_row("update t set a = (select b from c where d = 1)"), "the WHERE of a subquery is not its own");
        assert!(changes_every_row("update t set a = 'where'"));
        assert!(!changes_every_row("UPDATE t SET a = 1 WHERE id = 2"));
        assert!(!changes_every_row("select * from t"));
        assert!(changes_every_row("with x as (select 1) delete from t"));
    }

    #[test]
    fn what_writes_is_told_from_what_reads() {
        assert!(writes("INSERT INTO t VALUES (1) RETURNING id"));
        assert!(writes("with x as (select 1) delete from t returning id"));
        assert!(writes("CALL p()"));
        assert!(!writes("select * from t where a = 'delete'"));
        assert!(!writes("with x as (select 1) select * from x"));
    }

    #[test]
    fn parameters_are_found_outside_strings_and_comments() {
        let sql = "select ':no', a::int, @x := 1 /* :no */ from t where id = :id and d > :since or id = :id -- :no";
        assert_eq!(parameters(sql), ["id", "since"]);
    }

    #[test]
    fn a_value_goes_in_as_a_literal_and_cannot_end_its_string() {
        let sql = "select * from t where a = :a and b = :b and c = :c";
        let out = with_parameters(sql, Driver::MySql, |n| Some(match n { "a" => "12.5", "b" => "x' or '1'='1", _ => "null" }.to_string()));
        assert_eq!(out, "select * from t where a = 12.5 and b = 'x'' or ''1''=''1' and c = NULL");
        assert_eq!(literal("a\\", Driver::MySql), "'a\\\\'");
        assert_eq!(literal("a\\", Driver::Postgres), "'a\\'");
    }

    fn parts(text: &str) -> Vec<&str> {
        split(text).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn statements_part_at_a_semicolon_and_the_last_needs_none() {
        assert_eq!(parts("select 1;\n  select 2 ;\nselect 3"), ["select 1", "select 2", "select 3"]);
    }

    #[test]
    fn a_semicolon_in_a_string_a_name_or_a_comment_parts_nothing() {
        let text = "select ';', \"a;b\", `c;d` /* e;f */ from t -- g;h\n where x = 'it''s;' and y = 'a\\';b';select 2";
        let found = parts(text);
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("'a\\';b'"));
        assert_eq!(found[1], "select 2");
    }

    #[test]
    fn comments_around_a_statement_are_not_of_it_but_a_hint_is() {
        assert_eq!(parts("-- first\n# second\n/* third */ select 1 -- after\n;"), ["select 1"]);
        assert_eq!(parts("/*! set x = 1 */;"), ["/*! set x = 1 */"]);
        assert!(parts("-- only a comment\n").is_empty());
    }

    #[test]
    fn two_minuses_with_no_space_are_a_sum() {
        assert_eq!(parts("select 5--1; select 2"), ["select 5--1", "select 2"]);
    }

    #[test]
    fn delimiter_changes_what_ends_a_statement() {
        let text = "DELIMITER $$\ncreate procedure p() begin select 1; select 2; end$$\ndelimiter ;\nselect 3;";
        assert_eq!(parts(text), ["create procedure p() begin select 1; select 2; end", "select 3"]);
    }

    #[test]
    fn text_outside_ascii_keeps_its_boundaries() {
        assert_eq!(parts("select 'привет'; select имя"), ["select 'привет'", "select имя"]);
    }

    #[test]
    fn the_cursor_finds_its_statement() {
        let text = "select 1;  \n\nselect 2;\n";
        let all = split(text);
        let of = |cursor| at(text, &all, cursor).map(|r| text[r].to_string());
        assert_eq!(of(3).as_deref(), Some("select 1"));
        // Right after the semicolon, on the same line.
        assert_eq!(of(9).as_deref(), Some("select 1"));
        // On the empty line between them.
        assert_eq!(of(12).as_deref(), Some("select 2"));
        // At the end of the text.
        assert_eq!(of(text.len()).as_deref(), Some("select 2"));
        assert_eq!(at("", &[], 0), None);
    }
}
