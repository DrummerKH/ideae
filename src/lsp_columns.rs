//! Columns between the editor, which counts characters, and a server that
//! counts UTF-16 units because it was not able to agree on anything else.
//! They differ only after a character outside the BMP (an emoji), so only
//! the lines holding one are kept, not a copy of the text.

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::Value;

type WideLines = HashMap<u32, Box<str>>;

#[derive(Default)]
pub struct Columns {
    utf16: AtomicBool,
    /// By uri, every file open in the server, as last sent to it.
    open: Mutex<HashMap<String, WideLines>>,
}

fn wide_lines(text: &str) -> WideLines {
    if text.is_ascii() {
        return WideLines::new();
    }
    text.split('\n').enumerate().filter(|(_, line)| line.chars().any(|c| c.len_utf16() == 2)).map(|(row, line)| (row as u32, line.into())).collect()
}

/// A column past the end of the line stays past it by as much.
fn to_units(line: &str, column: u32) -> u32 {
    let (units, chars) = line.chars().take(column as usize).fold((0, 0), |(units, chars), c| (units + c.len_utf16() as u32, chars + 1));
    units + (column - chars)
}

/// Inside a surrogate pair, the character it is half of.
fn to_chars(line: &str, units: u32) -> u32 {
    let (mut seen, mut chars) = (0, 0);
    for c in line.chars() {
        let next = seen + c.len_utf16() as u32;
        if next > units {
            return chars;
        }
        (seen, chars) = (next, chars + 1);
    }
    chars + (units - seen)
}

/// A search can name thousands of files: past these, a column stays as told.
const FILES_OF_A_SEARCH: usize = 64;

/// The lines of the files an answer names. A file not open is read once,
/// up to `files` of them.
struct Lookup<'a> {
    columns: &'a Columns,
    files: usize,
    read: HashMap<String, WideLines>,
}

impl Lookup<'_> {
    fn line(&mut self, uri: &str, row: u32) -> Option<Box<str>> {
        if let Some(lines) = self.columns.open.lock().unwrap().get(uri) {
            return lines.get(&row).cloned();
        }
        if !self.read.contains_key(uri) && self.read.len() >= self.files {
            return None;
        }
        let lines = self.read.entry(uri.to_string()).or_insert_with(|| {
            crate::lsp::uri_to_path(uri).and_then(|path| std::fs::read_to_string(path).ok()).map_or_else(WideLines::new, |text| wide_lines(&text))
        });
        lines.get(&row).cloned()
    }
}

/// Every position in `value`, each in the file it is of: the nearest `uri`
/// around it, else `uri`. What a server keeps for itself is left as it wrote it.
fn walk(value: &mut Value, uri: &str, convert: &mut dyn FnMut(&str, u32, u32) -> u32) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(|item| walk(item, uri, convert)),
        Value::Object(map) => {
            if map.len() == 2 {
                if let (Some(row), Some(column)) = (map.get("line").and_then(Value::as_u64), map.get("character").and_then(Value::as_u64)) {
                    map.insert("character".into(), convert(uri, row as u32, column as u32).into());
                    return;
                }
            }
            let own = map.get("uri").or(map.get("targetUri")).or(map.get("textDocument").and_then(|document| document.get("uri"))).and_then(Value::as_str).map(str::to_string);
            let inner = own.as_deref().unwrap_or(uri);
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "data" | "command" | "arguments" => {}
                    // A link's own range is of the file asked about, not of its target.
                    "originSelectionRange" => walk(child, uri, convert),
                    "changes" if child.is_object() => {
                        if let Value::Object(files) = child {
                            for (file, edits) in files.iter_mut() {
                                walk(edits, file, convert);
                            }
                        }
                    }
                    _ => walk(child, inner, convert),
                }
            }
        }
        _ => {}
    }
}

impl Columns {
    /// From the answer to `initialize`: UTF-16 unless it says otherwise.
    pub fn agree(&self, capabilities: &Value) {
        self.utf16.store(capabilities["positionEncoding"].as_str().is_none_or(|encoding| encoding != "utf-32"), Ordering::Relaxed);
    }

    pub fn sent(&self, uri: &str, text: &str) {
        if self.utf16.load(Ordering::Relaxed) {
            let lines = wide_lines(text);
            self.open.lock().unwrap().insert(uri.to_string(), lines);
        }
    }

    pub fn closed(&self, uri: &str) {
        self.open.lock().unwrap().remove(uri);
    }

    pub fn to_server(&self, uri: &str, params: &mut Value) {
        if !self.utf16.load(Ordering::Relaxed) {
            return;
        }
        let mut lookup = Lookup { columns: self, files: usize::MAX, read: HashMap::new() };
        walk(params, uri, &mut |file, row, column| lookup.line(file, row).map_or(column, |line| to_units(&line, column)));
    }

    /// An answer to `method`, asked about `uri`.
    pub fn from_server(&self, uri: &str, method: &str, answer: &mut Value) {
        if !self.utf16.load(Ordering::Relaxed) {
            return;
        }
        if method.starts_with("textDocument/semanticTokens") {
            return self.tokens_from_server(uri, answer);
        }
        self.convert_from(uri, if method == "workspace/symbol" { FILES_OF_A_SEARCH } else { usize::MAX }, answer);
    }

    /// A report of problems, of a file open or not.
    pub fn problems_from_server(&self, uri: &str, problems: &mut Value) {
        if self.utf16.load(Ordering::Relaxed) {
            self.convert_from(uri, usize::MAX, problems);
        }
    }

    fn convert_from(&self, uri: &str, files: usize, value: &mut Value) {
        let mut lookup = Lookup { columns: self, files, read: HashMap::new() };
        walk(value, uri, &mut |file, row, column| lookup.line(file, row).map_or(column, |line| to_chars(&line, column)));
    }

    /// Tokens are five numbers each, a start relative to the one before.
    fn tokens_from_server(&self, uri: &str, answer: &mut Value) {
        let Some(data) = answer.get_mut("data").and_then(Value::as_array_mut) else { return };
        let mut lookup = Lookup { columns: self, files: 0, read: HashMap::new() };
        let (mut row, mut units, mut last_row, mut last_start) = (0u32, 0u32, 0u32, 0u32);
        for token in data.chunks_mut(5) {
            let [delta_row, delta_start, length, ..] = token else { break };
            let number = |value: &Value| value.as_u64().unwrap_or(0) as u32;
            let (down, across, long) = (number(delta_row), number(delta_start), number(length));
            if down > 0 {
                (row, units) = (row + down, across);
            } else {
                units += across;
            }
            let (start, chars) = match lookup.line(uri, row) {
                Some(line) => {
                    let start = to_chars(&line, units);
                    (start, to_chars(&line, units + long).saturating_sub(start))
                }
                None => (units, long),
            };
            *delta_row = (row - last_row).into();
            *delta_start = (if row == last_row { start.saturating_sub(last_start) } else { start }).into();
            *length = chars.into();
            (last_row, last_start) = (row, start);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FILE: &str = "file:///a.py";
    const OTHER: &str = "file:///b.py";

    fn columns() -> Columns {
        let columns = Columns::default();
        columns.agree(&json!({}));
        columns.sent(FILE, "plain\nx = '😀é'; name\n");
        columns.sent(OTHER, "😀😀 other\n");
        columns
    }

    #[test]
    fn columns_are_counted_either_way_at_every_place_of_a_line() {
        let line = "a😀é中𝒳b";
        for (column, _) in line.chars().enumerate() {
            let column = column as u32;
            assert_eq!(to_chars(line, to_units(line, column)), column);
        }
        assert_eq!(to_units(line, 2), 3);
        assert_eq!(to_chars(line, 2), 1, "half of a pair is the character it is of");
        assert_eq!(to_chars(line, to_units(line, 40)), 40, "past the end stays past it");
    }

    #[test]
    fn a_server_that_agreed_on_characters_is_left_alone() {
        let columns = Columns::default();
        columns.agree(&json!({ "positionEncoding": "utf-32" }));
        columns.sent(FILE, "😀 x\n");
        let mut value = json!({ "textDocument": { "uri": FILE }, "position": { "line": 0, "character": 2 } });
        let before = value.clone();
        columns.to_server(FILE, &mut value);
        columns.from_server(FILE, "textDocument/hover", &mut value);
        assert_eq!(value, before);
    }

    #[test]
    fn a_question_and_its_answer_are_each_in_the_server_s_and_the_editor_s_columns() {
        let columns = columns();
        let mut asked = json!({ "textDocument": { "uri": FILE }, "position": { "line": 1, "character": 11 } });
        columns.to_server(FILE, &mut asked);
        assert_eq!(asked["position"]["character"], 12);
        let mut answer = json!({ "range": { "start": { "line": 1, "character": 12 }, "end": { "line": 1, "character": 16 } } });
        columns.from_server(FILE, "textDocument/hover", &mut answer);
        assert_eq!((&answer["range"]["start"]["character"], &answer["range"]["end"]["character"]), (&json!(11), &json!(15)));
    }

    #[test]
    fn each_edit_is_counted_in_its_own_file_and_what_the_server_keeps_is_not_touched() {
        let columns = columns();
        let at = |c: u32| json!({ "start": { "line": 0, "character": c }, "end": { "line": 0, "character": c } });
        let mut answer = json!({
            "changes": { OTHER: [{ "range": at(5), "newText": "n" }], FILE: [{ "range": at(5), "newText": "n" }] },
            "documentChanges": [{ "textDocument": { "uri": OTHER }, "edits": [{ "range": at(5), "newText": "n" }] }],
            "data": { "range": at(5) },
        });
        columns.from_server(FILE, "textDocument/rename", &mut answer);
        assert_eq!(answer["changes"][OTHER][0]["range"]["start"]["character"], 3);
        assert_eq!(answer["changes"][FILE][0]["range"]["start"]["character"], 5, "no emoji on that line");
        assert_eq!(answer["documentChanges"][0]["edits"][0]["range"]["start"]["character"], 3);
        assert_eq!(answer["data"]["range"]["start"]["character"], 5);
    }

    #[test]
    fn a_link_s_origin_is_of_the_file_asked_about() {
        let columns = columns();
        let range = |c: u32| json!({ "start": { "line": 0, "character": c }, "end": { "line": 0, "character": c } });
        let mut answer = json!([{ "originSelectionRange": range(5), "targetUri": OTHER, "targetRange": range(5), "targetSelectionRange": range(5) }]);
        columns.from_server(FILE, "textDocument/definition", &mut answer);
        assert_eq!(answer[0]["originSelectionRange"]["start"]["character"], 5);
        assert_eq!(answer[0]["targetRange"]["start"]["character"], 3);
    }

    #[test]
    fn tokens_after_an_emoji_stay_on_their_names() {
        let columns = columns();
        // `x` at 0, `name` at 12 units of line 1; `other` at 5 of the file's next line none.
        let mut answer = json!({ "data": [1, 0, 1, 0, 0, 0, 12, 4, 1, 0] });
        columns.from_server(FILE, "textDocument/semanticTokens/full", &mut answer);
        assert_eq!(answer["data"], json!([1, 0, 1, 0, 0, 0, 11, 4, 1, 0]));
    }

    #[test]
    fn a_file_not_open_is_read_for_its_columns() {
        let dir = std::env::temp_dir().join(format!("ideae-columns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("closed.py");
        std::fs::write(&file, "😀 name\n").unwrap();
        let uri = crate::lsp::path_to_uri(&file);
        let columns = columns();
        let mut problem = json!({ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 7 } } });
        columns.problems_from_server(&uri, &mut problem);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(problem["range"]["start"]["character"], 2);
    }

    #[test]
    fn what_is_closed_or_changed_is_not_counted_by_its_old_text() {
        let columns = columns();
        columns.sent(FILE, "plain\nno emoji here now\n");
        let mut answer = json!({ "line": 1, "character": 12 });
        columns.from_server(FILE, "textDocument/hover", &mut answer);
        assert_eq!(answer["character"], 12);
        columns.closed(OTHER);
        // Its file is not on the disk: nothing to count by.
        let mut problem = json!({ "range": { "start": { "line": 0, "character": 5 }, "end": { "line": 0, "character": 5 } } });
        columns.problems_from_server(OTHER, &mut problem);
        assert_eq!(problem["range"]["start"]["character"], 5);
    }
}
