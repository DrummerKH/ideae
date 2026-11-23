//! The structure of a file: what is declared in it, by a small tree-sitter
//! query per language (`queries/outline/`), with the regions its comments
//! mark. A pattern captures a declaration as its kind (`@class`, `@enum_variant`)
//! and what names it as `@name`, in as many parts as it takes.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex};

use gpui_kit::component::highlighter::{LanguageRegistry, text_regions};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::finder::OutlineItem;
use crate::lsp;

macro_rules! queries {
    ($($language:literal => $file:literal),* $(,)?) => {
        fn query_source(language: &str) -> Option<&'static str> {
            match language {
                $($language => Some(include_str!(concat!("../../queries/outline/", $file, ".scm"))),)*
                _ => None,
            }
        }
        #[cfg(test)]
        const LANGUAGES: &[&str] = &[$($language),*];
    };
}

queries! {
    "astro" => "astro",
    "bash" => "bash",
    "batch" => "batch",
    "c" => "c",
    "cmake" => "cmake",
    "cpp" => "cpp",
    "csharp" => "csharp",
    "css" => "css",
    "dart" => "dart",
    "diff" => "diff",
    "dockerfile" => "dockerfile",
    "elixir" => "elixir",
    "elm" => "elm",
    "erlang" => "erlang",
    "fish" => "fish",
    "gleam" => "gleam",
    "glsl" => "glsl",
    "go" => "go",
    "graphql" => "graphql",
    "groovy" => "groovy",
    "haskell" => "haskell",
    "hcl" => "hcl",
    "html" => "html",
    "ini" => "ini",
    "java" => "java",
    "javascript" => "javascript",
    "json" => "json",
    "julia" => "julia",
    "kotlin" => "kotlin",
    "less" => "less",
    "lua" => "lua",
    "make" => "make",
    "markdown" => "markdown",
    "nginx" => "nginx",
    "nix" => "nix",
    "ocaml" => "ocaml",
    "php" => "php",
    "powershell" => "powershell",
    "prisma" => "prisma",
    "properties" => "properties",
    "proto" => "proto",
    "python" => "python",
    "r" => "r",
    "ruby" => "ruby",
    "rust" => "rust",
    "scala" => "scala",
    "solidity" => "solidity",
    "svelte" => "svelte",
    "swift" => "swift",
    "toml" => "toml",
    "twig" => "twig",
    "typescript" => "typescript",
    "xml" => "xml",
    "yaml" => "yaml",
    "zig" => "zig",
    "tsx" => "typescript",
}

/// Compiled once: the project's symbols run this over thousands of files.
static COMPILED: LazyLock<Mutex<HashMap<&'static str, Option<Arc<Query>>>>> = LazyLock::new(Default::default);

fn query(language: &'static str, grammar: &tree_sitter::Language) -> Option<Arc<Query>> {
    let mut compiled = COMPILED.lock().unwrap_or_else(|e| e.into_inner());
    compiled.entry(language).or_insert_with(|| Query::new(grammar, query_source(language)?).ok().map(Arc::new)).clone()
}

/// One thing of the outline before it is nested.
struct Entry {
    kind: String,
    name: String,
    bytes: Range<usize>,
    line: u32,
    end: u32,
    private: bool,
}

/// What is declared in a file, nested as in the source, with its regions.
pub(super) fn outline(path: &std::path::Path, text: &str) -> Vec<OutlineItem> {
    let language = lsp::language_for(path);
    let mut entries = if language == "sql" { sql_statements(text) } else { declarations(language, text) };
    entries.extend(text_regions(language, text).into_iter().map(|region| Entry {
        kind: REGION.to_string(),
        name: if region.name.is_empty() { "region".to_string() } else { region.name },
        bytes: region.bytes,
        line: region.line as u32,
        end: region.end_line as u32,
        private: false,
    }));
    nest(entries)
}

pub(super) const REGION: &str = "region";

/// Whether an outline's item is declared in the code, for the project's
/// symbols: a file's headings, keys, statements and regions are not.
pub(super) fn is_declaration(kind: &str) -> bool {
    !matches!(kind, REGION | "heading" | "key" | "tag" | "section" | "statement" | "selector" | "at rule" | "stage" | "target" | "hunk" | "label")
}

fn nest(mut entries: Vec<Entry>) -> Vec<OutlineItem> {
    // A region that begins inside a declaration and ends past it holds
    // neither: it is a mark at its first line.
    let crossing: Vec<usize> = (0..entries.len())
        .filter(|&ix| entries[ix].kind == REGION)
        .filter(|&ix| {
            let region = &entries[ix].bytes;
            entries.iter().any(|other| {
                let o = &other.bytes;
                (o.start < region.start && region.start < o.end && o.end < region.end)
                    || (region.start < o.start && o.start < region.end && region.end < o.end)
            })
        })
        .collect();
    for ix in crossing {
        let entry = &mut entries[ix];
        entry.bytes.end = entry.bytes.start + 1;
        entry.end = entry.line;
    }
    entries.sort_by_key(|entry| (entry.bytes.start, std::cmp::Reverse(entry.bytes.end)));
    let mut open: Vec<Range<usize>> = Vec::new();
    entries
        .into_iter()
        .map(|entry| {
            // Names declared by one node (`int a, b`) are side by side.
            while open.last().is_some_and(|at: &Range<usize>| at.end <= entry.bytes.start || *at == entry.bytes) {
                open.pop();
            }
            let depth = open.len();
            open.push(entry.bytes.clone());
            OutlineItem { depth, kind: entry.kind, name: entry.name, line: entry.line, end: entry.end, private: entry.private }
        })
        .collect()
}

fn declarations(language: &'static str, text: &str) -> Vec<Entry> {
    let Some(grammar) = LanguageRegistry::singleton().language(language).and_then(|config| config.language) else {
        return Vec::new();
    };
    let Some(query) = query(language, &grammar) else { return Vec::new() };
    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else { return Vec::new() };
    let names = query.capture_names();
    let name_ix = query.capture_index_for_name("name");
    // A node two patterns take is the first pattern's; one pattern may find
    // several names on it (`int a, b`).
    let mut found: HashMap<(usize, usize), (usize, Vec<Entry>)> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
    while let Some(found_match) = matches.next() {
        let mut parts: Vec<Node> = found_match.captures.iter().filter(|c| Some(c.index) == name_ix).map(|c| c.node).collect();
        let Some(item) = found_match.captures.iter().find(|c| Some(c.index) != name_ix && !names[c.index as usize].starts_with('_')) else { continue };
        parts.sort_by_key(Node::start_byte);
        parts.dedup_by_key(|n| n.id());
        let mut name = parts.iter().filter_map(|n| text.get(n.byte_range())).map(clean_name).collect::<Vec<_>>().join(" ");
        let Some(first) = parts.first().filter(|_| !name.is_empty()) else { continue };
        let node = item.node;
        let kind = names[item.index as usize].replace(['_', '.'], " ");
        if kind == "companion object" && name == "companion" {
            name = "Companion".to_string();
        }
        let entry = Entry {
            private: is_private(language, node, &name, text),
            kind,
            name,
            bytes: node.byte_range(),
            line: first.start_position().row as u32,
            end: last_line(node) as u32,
        };
        let pattern = found_match.pattern_index;
        let slot = found.entry((node.start_byte(), node.end_byte())).or_insert((pattern, Vec::new()));
        if pattern < slot.0 {
            *slot = (pattern, vec![entry]);
        } else if pattern == slot.0 && !slot.1.iter().any(|known| known.name == entry.name) {
            slot.1.push(entry);
        }
    }
    found.into_values().flat_map(|(_, entries)| entries).collect()
}

/// A name's first line, without the quotes of a key.
fn clean_name(name: &str) -> String {
    let line = name.lines().next().unwrap_or(name).trim();
    line.trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string()
}

/// A YAML block ends past its last line break, at the start of the next.
fn last_line(node: Node) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row { end.row - 1 } else { end.row }
}

/// A `.sql` file's statements: a verb and what it is about; `create` names
/// what it makes, and only that is a declaration.
fn sql_statements(text: &str) -> Vec<Entry> {
    let (mut counted, mut line) = (0, 0u32);
    crate::db::statement::split(text)
        .into_iter()
        .filter_map(|range| {
            line += text.as_bytes()[counted..range.start].iter().filter(|b| **b == b'\n').count() as u32;
            counted = range.start;
            let sql = text.get(range.clone())?;
            let (kind, name) = sql_label(sql)?;
            Some(Entry { kind, name, line, end: line + sql.matches('\n').count() as u32, bytes: range, private: false })
        })
        .collect()
}

/// `create table a` → ("table", "a"), `select … from a` → ("statement", "select a").
fn sql_label(sql: &str) -> Option<(String, String)> {
    let words: Vec<&str> = sql.split_whitespace().collect();
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let verb = lower.first()?.as_str();
    let clean = |word: &str| word.trim_matches(|c: char| matches!(c, '`' | '"' | '[' | ']' | '(' | ')' | ',' | ';')).to_string();
    if verb == "create" {
        const SKIP: &[&str] = &["or", "replace", "temporary", "temp", "unique", "materialized", "if", "not", "exists", "global", "local", "unlogged", "recursive"];
        const OBJECTS: &[&str] = &["table", "view", "index", "function", "procedure", "trigger", "database", "schema", "type", "sequence", "event", "extension"];
        let object = lower.iter().position(|w| OBJECTS.contains(&w.as_str()));
        let named = object.and_then(|at| Some((at, lower[at + 1..].iter().position(|w| !SKIP.contains(&w.as_str()))? + at + 1)));
        if let Some((at, name)) = named {
            let name = clean(words[name].split('(').next().unwrap_or(words[name]));
            if !name.is_empty() {
                return Some((lower[at].clone(), name));
            }
        }
    }
    let after = |word: &str| lower.iter().position(|w| w == word).and_then(|at| words.get(at + 1)).map(|w| clean(w));
    let object = match verb {
        "select" => after("from"),
        "insert" | "replace" => after("into"),
        "delete" => after("from"),
        "update" | "set" | "call" | "use" | "show" | "describe" | "desc" | "explain" => words.get(1).map(|w| clean(w)),
        "alter" | "drop" | "truncate" => {
            const SKIP: &[&str] = &["table", "view", "index", "function", "procedure", "trigger", "database", "schema", "if", "exists"];
            lower.iter().skip(1).position(|w| !SKIP.contains(&w.as_str())).and_then(|at| words.get(at + 1)).map(|w| clean(w))
        }
        _ => None,
    };
    let object = object.filter(|o| !o.is_empty());
    let name = match object {
        Some(object) => format!("{verb} {object}"),
        None => words.iter().take(4).copied().collect::<Vec<_>>().join(" "),
    };
    Some(("statement".to_string(), name))
}

/// Whether a declaration is private, read from the syntax tree: the
/// modifier the grammar has on it (`private`, Rust's missing `pub`), not
/// the text of its line. Two languages have no such word and say it by the
/// name: Go's lower case, Python's underscore.
fn is_private(language: &str, node: Node, name: &str, text: &str) -> bool {
    // The modifiers a grammar hangs on a declaration, whatever it calls them: `visibility_modifier`, `modifiers`, `accessibility_modifier`.
    let mut cursor = node.walk();
    let modifiers: Vec<&str> = node
        .children(&mut cursor)
        .filter(|child| {
            let kind = child.kind();
            kind.contains("visibility") || kind.contains("accessibility") || kind.contains("modifier")
        })
        .filter_map(|child| text.get(child.byte_range()))
        .collect();
    let says = |word: &str| modifiers.iter().any(|modifier| modifier.split(|c: char| !c.is_alphanumeric()).any(|w| w == word));
    match language {
        "rust" => {
            // What a trait asks for, and what an impl gives it, is as public as the trait; so are an enum's variants. An impl itself is of its type.
            let kind = node.kind();
            let of_a_trait = std::iter::successors(node.parent(), |up| up.parent())
                .any(|up| up.kind() == "trait_item" || (up.kind() == "impl_item" && up.child_by_field_name("trait").is_some()) || up.kind() == "enum_variant_list");
            !says("pub") && !of_a_trait && kind != "impl_item"
        }
        "go" => name.chars().next().is_some_and(|c| c.is_lowercase() || c == '_'),
        "python" => name.starts_with('_') && !name.starts_with("__init__"),
        "elixir" => text.get(node.byte_range()).is_some_and(|code| code.starts_with("defp")),
        _ => says("private") || name.starts_with('#'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// `queries/outline/cases.txt`: a file of each language and its outline.
    fn cases() -> Vec<(&'static str, String, String)> {
        include_str!("../../queries/outline/cases.txt")
            .split("=== ")
            .skip(1)
            .map(|case| {
                let (name, rest) = case.split_once('\n').unwrap();
                let (code, wanted) = rest.split_once("@@@\n").unwrap_or((rest, ""));
                (name.trim(), code.to_string(), wanted.trim_end().to_string())
            })
            .collect()
    }

    fn shown(path: &str, text: &str) -> String {
        outline(Path::new(path), text).iter().map(|i| format!("{}{} {} :{}", "  ".repeat(i.depth), i.kind, i.name, i.line + 1)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn every_language_has_the_outline_written_for_it() {
        crate::grammars::register();
        let mut wrong = Vec::new();
        for (name, code, wanted) in cases() {
            let found = shown(name, &code);
            if found != wanted {
                wrong.push(format!("=== {name}\n{found}\n--- wanted\n{wanted}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn a_region_across_a_declaration_holds_nothing() {
        let text = "class A {\n  // region half\n  void a() {}\n}\nclass B {}\n// endregion\n";
        assert_eq!(shown("A.java", text), "class A :1\n  region half :2\n  method a :3\nclass B :5");
    }

    #[test]
    fn what_a_file_only_shows_is_not_among_the_project_s_symbols() {
        for kind in ["region", "heading", "key", "statement", "selector"] {
            assert!(!is_declaration(kind), "{kind}");
        }
        for kind in ["function", "class", "table", "view"] {
            assert!(is_declaration(kind), "{kind}");
        }
    }

    #[test]
    fn a_statement_is_told_by_its_verb_and_what_it_is_on() {
        assert_eq!(sql_label("create temporary table if not exists `a`(id int)"), Some(("table".into(), "a".into())));
        assert_eq!(sql_label("CREATE OR REPLACE VIEW v AS select 1"), Some(("view".into(), "v".into())));
        assert_eq!(sql_label("select a,\n b from t where c"), Some(("statement".into(), "select t".into())));
        assert_eq!(sql_label("drop table if exists t"), Some(("statement".into(), "drop t".into())));
        assert_eq!(sql_label("with x as (select 1) select * from x").map(|l| l.0), Some("statement".into()));
    }

    #[test]
    fn what_is_private_is_told_by_its_words() {
        let private = |path: &str, text: &str| -> Vec<String> {
            outline(Path::new(path), text).into_iter().filter(|i| i.private).map(|i| i.name).collect()
        };
        let rust = "pub struct Open {}\nstruct Kept {}\nimpl Open {\n    pub fn seen(&self) {}\n    fn hidden(&self) {}\n}\npub(crate) fn near() {}\n\
            pub trait Says {\n    fn say(&self);\n}\nimpl Says for Open {\n    fn say(&self) {}\n}\n";
        assert_eq!(private("a.rs", rust), ["Kept", "hidden"], "a trait's own, and what an impl gives it, are as public as the trait");
        let ts = "class A {\n  open() {}\n  private kept() {}\n  #hash() {}\n}\n";
        assert_eq!(private("a.ts", ts), ["kept", "#hash"]);
        let kt = "class A {\n    fun open() {}\n    private fun kept() {}\n}\n";
        assert_eq!(private("a.kt", kt), ["kept"]);
        assert_eq!(private("m.go", "package m\n\nfunc Open() {}\n\nfunc kept() {}\n"), ["kept"]);
        assert_eq!(private("a.py", "def open():\n    pass\n\ndef _kept():\n    pass\n"), ["_kept"]);
        let java = "class A {\n    public void open() {}\n    private void kept() {}\n}\n";
        assert_eq!(private("A.java", java), ["kept"]);
    }

    #[test]
    fn go_declarations() {
        let src = "package main\n\ntype Server struct{}\n\nfunc (s *Server) Run() { go s.loop() }\n\nfunc main() {}\n";
        let labels: Vec<String> = outline(Path::new("m.go"), src).into_iter().map(|i| format!("{} {}", i.kind, i.name)).collect();
        assert!(labels.iter().any(|l| l.ends_with("Server")), "{labels:?}");
        assert!(labels.iter().any(|l| l.ends_with("Run")), "{labels:?}");
        assert!(labels.iter().any(|l| l.ends_with("main")), "{labels:?}");
        assert!(!labels.iter().any(|l| l.contains("loop")), "{labels:?}");
    }

    #[test]
    fn fields_are_listed() {
        let ts = "class A {\n  count: number = 1;\n  name = 'x';\n  run() {}\n}\n";
        let labels: Vec<String> = outline(Path::new("a.ts"), ts).into_iter().map(|i| format!("{} {}", i.kind, i.name)).collect();
        assert!(labels.iter().any(|l| l.ends_with("count")), "{labels:?}");
        assert!(labels.iter().any(|l| l.ends_with("run")), "{labels:?}");
        let kt = "class A {\n    val count: Int = 1\n    fun run() {}\n}\n";
        let labels: Vec<String> = outline(Path::new("a.kt"), kt).into_iter().map(|i| format!("{} {}", i.kind, i.name)).collect();
        assert!(labels.iter().any(|l| l.ends_with("count")), "{labels:?}");
    }

    fn lines(path: &str, text: &str) -> Vec<(String, u32, u32)> {
        crate::grammars::register();
        outline(Path::new(path), text).into_iter().map(|i| (format!("{}{} {}", "  ".repeat(i.depth), i.kind, i.name), i.line, i.end)).collect()
    }

    #[test]
    fn patterns_calls_and_clauses_are_not_declarations() {
        let rust = "struct P { x: u32 }\nfn f(o: Option<P>) {\n    match o { Some(P { x }) => {}, None => {} }\n    if let Ok(v) = parse::<u32>(\"1\") {}\n}\nimpl<T: 'static> Tr for P {}\n";
        assert_eq!(
            lines("a.rs", rust).into_iter().map(|(l, _, _)| l).collect::<Vec<_>>(),
            ["struct P", "  field x", "function f", "impl Tr for P"]
        );
        let ts = "export const run = async (req) => { list.map((item) => item.id); return { ok: true }; };\nclass A extends B implements C {}\n";
        assert_eq!(lines("a.ts", ts).into_iter().map(|(l, _, _)| l).collect::<Vec<_>>(), ["function run", "class A"]);
        let kt = "class A(val p: Int) : B() {\n    fun run() {}\n}\n";
        assert_eq!(lines("a.kt", kt).into_iter().map(|(l, _, _)| l).collect::<Vec<_>>(), ["class A", "  function run"]);
    }

    #[test]
    fn kotlin_objects_are_declarations() {
        let kt = "object Registry {\n    fun get() {}\n}\nclass A {\n    companion object {\n        const val X = 1\n    }\n    companion object Factory\n}\n";
        assert_eq!(
            lines("a.kt", kt).into_iter().map(|(l, _, _)| l).collect::<Vec<_>>(),
            ["object Registry", "  function get", "class A", "  companion object Companion", "    property X", "  companion object Factory"]
        );
    }

    #[test]
    fn an_annotated_class_starts_at_its_name() {
        let text = "@Obfuscate\nabstract class App : Base() {\n    @Suppress(\"x\")\n    fun run() {\n    }\n}\n";
        assert_eq!(lines("a.kt", text)[0].1, 1);
        assert!(lines("a.kt", text).iter().any(|(l, line, _)| l.ends_with("function run") && *line == 3), "{:?}", lines("a.kt", text));
    }

    #[test]
    fn yaml_json_and_xml_have_their_keys_and_tags() {
        let yaml = lines("a.yml", "services:\n  web:\n    image: x\n");
        assert_eq!(yaml, [("key services".into(), 0, 2), ("  key web".into(), 1, 2), ("    key image".into(), 2, 2)]);
        let json = lines("a.json", "{\n  \"a\": {\n    \"b\": 1\n  }\n}\n");
        assert_eq!(json, [("key a".into(), 1, 3), ("  key b".into(), 2, 2)]);
        let xml = lines("a.xml", "<?xml version=\"1.0\"?>\n<project>\n  <dependency/>\n</project>\n");
        assert_eq!(xml, [("tag project".into(), 1, 3), ("  tag dependency".into(), 2, 2)]);
    }

    #[test]
    fn every_query_compiles() {
        crate::grammars::register();
        let mut failed = Vec::new();
        for language in LANGUAGES {
            let grammar = LanguageRegistry::singleton().language(language).and_then(|c| c.language).unwrap_or_else(|| panic!("{language}: no grammar"));
            if let Err(e) = Query::new(&grammar, query_source(language).unwrap()) {
                failed.push(format!("{language}: {e}"));
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

}
