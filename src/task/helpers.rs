use super::*;

/// A `git diff` as the panels need it, made off the UI thread.
pub(super) struct ParsedPatch {
    pub(super) files: Vec<DiffFile>,
    pub(super) first_lines: HashMap<String, u32>,
    pub(super) hashes: HashMap<String, u64>,
    pub(super) hunks: Vec<git::Hunk>,
    /// Files with a conflict left in them, and its line.
    pub(super) marked: Vec<(String, u32)>,
}

pub(super) fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Parses `patch` unless it is the one seen last (`seen`, its hash).
pub(super) fn parse_patch(patch: String, seen: &mut u64) -> Option<ParsedPatch> {
    let hash = text_hash(&patch);
    if std::mem::replace(seen, hash) == hash {
        return None;
    }
    let files = DiffFile::parse(&patch).inspect_err(|error| eprintln!("diff parse: {error}")).ok()?;
    Some(ParsedPatch {
        first_lines: git::first_changed_lines(&patch),
        hashes: file_patch_hashes(&patch),
        hunks: git::hunks(&patch),
        marked: git::conflict_marked(&patch),
        files: with_languages(files),
    })
}

/// Each file's part of a `git diff`, hashed, by its path after the change.
pub(super) fn file_patch_hashes(patch: &str) -> HashMap<String, u64> {
    use std::hash::{Hash as _, Hasher as _};
    let mut out = HashMap::new();
    let mut current: Option<(String, std::collections::hash_map::DefaultHasher)> = None;
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            if let Some((path, hasher)) = current.take() {
                out.insert(path, hasher.finish());
            }
            // "a/x b/x": the path after " b/".
            let path = header.rsplit_once(" b/").map_or(header, |(_, b)| b).to_string();
            current = Some((path, std::collections::hash_map::DefaultHasher::new()));
        } else if let Some((_, hasher)) = current.as_mut() {
            line.hash(hasher);
        }
    }
    if let Some((path, hasher)) = current {
        out.insert(path, hasher.finish());
    }
    out
}

/// A glyph and its color for the agent status, if it has one to show.
/// `http://localhost:3100` of `http://localhost:3100/users?page=2`.
pub(super) fn origin(url: &str) -> &str {
    let after_scheme = url.find("://").map_or(0, |at| at + 3);
    let end = url[after_scheme..].find(['/', '?', '#']).map_or(url.len(), |at| after_scheme + at);
    &url[..end]
}

/// What became of a message to an agent.
#[derive(Clone, Copy, PartialEq)]
pub enum Told {
    Now,
    /// When its turn ends.
    Later,
    /// It is not up.
    Not,
}

pub fn status_glyph(status: &crate::agent::AgentStatus) -> Option<(&'static str, Hsla)> {
    use crate::agent::AgentStatus::*;
    Some(match status {
        Unknown => return None,
        Idle => ("○", hsla(0., 0., 0.55, 1.)),
        Working => ("●", hsla(0.58, 0.8, 0.55, 1.)),
        NeedsInput(_) => ("!", hsla(0.08, 0.9, 0.55, 1.)),
        Done => ("✓", hsla(0.33, 0.6, 0.45, 1.)),
        Exited => ("×", hsla(0., 0., 0.55, 1.)),
    })
}

/// Highlights diffs with the editor's language table, which knows more
/// (Kotlin aliases, XML, Gradle, properties) than the diff's own detection.
/// A diff of its own in a dialog: a stash, two variants against each other.
pub fn show_diff_dialog(title: String, files: Vec<DiffFile>, window: &mut Window, cx: &mut App) {
    if files.is_empty() {
        return window.push_notification(Notification::info(format!("{title}: nothing differs.")), cx);
    }
    let diff = cx.new(|cx| DiffState::new(files, cx).with_mode(DiffMode::Split));
    let room = window.viewport_size();
    let (width, height) = (room.width * 0.92, room.height * 0.72);
    window.open_dialog(cx, move |dialog, _, _| {
        let diff = diff.clone();
        dialog.title(title.clone()).w(width).content(move |content, _, cx| {
            content.child(div().h(height).child(Diff::new(&diff).line_number(true).code_font_size(crate::zoom::size(crate::zoom::Kind::Changes, cx))))
        })
    });
}

pub fn with_languages(files: Vec<DiffFile>) -> Vec<DiffFile> {
    files
        .into_iter()
        .map(|f| match lsp::language_for(std::path::Path::new(f.path().as_ref())) {
            "text" => f,
            language => f.with_language(language),
        })
        .collect()
}

pub(super) fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// The smallest syntax node strictly larger than `range`, by the file's
/// tree-sitter grammar.
pub(super) fn syntax_parent(path: &std::path::Path, text: &str, range: std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
    let config = gpui_kit::component::highlighter::LanguageRegistry::singleton().language(lsp::language_for(path))?;
    let language = config.language?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(text, None)?;
    let mut node = tree.root_node().descendant_for_byte_range(range.start, range.end)?;
    loop {
        let r = node.byte_range();
        let larger = r.start <= range.start && r.end >= range.end && r != range;
        // Skip zero-width and whitespace-only steps: they look like no change.
        if larger && !text[r.clone()].trim().is_empty() {
            return Some(r);
        }
        node = node.parent()?;
    }
}

/// Without a grammar: the word, then the line, then everything.
pub(super) fn plain_parent(text: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let line_start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[range.end..].find('\n').map_or(text.len(), |i| range.end + i);
    if range.is_empty() {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let start = text[..range.start].rfind(|c: char| !is_word(c)).map_or(0, |i| i + 1);
        let end = text[range.end..].find(|c: char| !is_word(c)).map_or(text.len(), |i| range.end + i);
        if start < end {
            return start..end;
        }
    }
    if range != (line_start..line_end) && range.start >= line_start && range.end <= line_end {
        return line_start..line_end;
    }
    0..text.len()
}

/// The line comment of a language, or the block markers wrapped around
/// lines for XML-like ones.
pub(super) fn comment_markers(path: &std::path::Path) -> (&'static str, &'static str) {
    match lsp::language_for(path) {
        "go" | "rust" | "kotlin" | "java" | "swift" | "scala" | "c" | "cpp" | "csharp" | "typescript" | "tsx"
        | "javascript" | "php" | "groovy" | "zig" | "proto" | "css" => ("// ", ""),
        "sql" | "lua" => ("-- ", ""),
        "ini" => ("; ", ""),
        "batch" => ("REM ", ""),
        "xml" | "html" | "markdown" | "svelte" => ("<!-- ", " -->"),
        _ => ("# ", ""),
    }
}

/// The byte range of the selected lines and their text with comments
/// toggled: removed when every non-blank line has one, else added at the
/// common indentation.
pub(super) fn toggle_comment_lines(path: &std::path::Path, text: &str, range: std::ops::Range<usize>) -> Option<(usize, usize, String)> {
    let (open, close) = comment_markers(path);
    let start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    // A selection ending at a line start does not take that line.
    let last = if range.end > range.start && text[..range.end].ends_with('\n') { range.end - 1 } else { range.end };
    let end = text[last..].find('\n').map_or(text.len(), |i| last + i);
    let lines: Vec<&str> = text[start..end].split('\n').collect();
    let open_trim = open.trim_end();
    let close_trim = close.trim_start();
    let commented = |l: &str| {
        let t = l.trim_start();
        t.starts_with(open_trim) && (close_trim.is_empty() || t.trim_end().ends_with(close_trim))
    };
    let all = lines.iter().filter(|l| !l.trim().is_empty()).all(|l| commented(l))
        && lines.iter().any(|l| !l.trim().is_empty());
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let out: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                return l.to_string();
            }
            if all {
                let lead = l.len() - l.trim_start().len();
                let mut body = &l[lead..];
                body = body.strip_prefix(open).or_else(|| body.strip_prefix(open_trim)).unwrap_or(body);
                if !close_trim.is_empty() {
                    let trimmed = body.trim_end();
                    body = trimmed.strip_suffix(close).or_else(|| trimmed.strip_suffix(close_trim)).unwrap_or(body);
                }
                format!("{}{}", &l[..lead], body)
            } else {
                format!("{}{}{}{}", &l[..indent], open, &l[indent..], close)
            }
        })
        .collect();
    Some((start, end, out.join("\n")))
}

#[cfg(test)]
mod editing_tests {
    use super::{plain_parent, toggle_comment_lines};
    use std::path::Path;

    #[test]
    fn comment_toggles_both_ways() {
        let text = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
        let a = text.find("let a").unwrap();
        let b_end = text.find("2;").unwrap() + 2;
        let (s, e, out) = toggle_comment_lines(Path::new("x.rs"), text, a..b_end).unwrap();
        assert_eq!(out, "    // let a = 1;\n    // let b = 2;");
        let commented = format!("{}{}{}", &text[..s], out, &text[e..]);
        let (s2, e2, back) = toggle_comment_lines(Path::new("x.rs"), &commented, s..s + out.len()).unwrap();
        assert_eq!(format!("{}{}{}", &commented[..s2], back, &commented[e2..]), text);
    }

    #[test]
    fn xml_wraps_lines() {
        let (_, _, out) = toggle_comment_lines(Path::new("a.xml"), "<a/>", 0..0).unwrap();
        assert_eq!(out, "<!-- <a/> -->");
    }

    #[test]
    fn plain_expansion_steps_out() {
        let text = "alpha beta\ngamma";
        assert_eq!(plain_parent(text, 7..7), 6..10);
        assert_eq!(plain_parent(text, 6..10), 0..10);
        assert_eq!(plain_parent(text, 0..10), 0..text.len());
    }
}

/// Declarations in a file, by its tree-sitter grammar: classes, functions,
/// methods, types… nested as in the source. Calls and expressions are left
/// out even when the grammar gives them a name.
pub(super) fn outline(path: &std::path::Path, text: &str) -> Vec<crate::finder::OutlineItem> {
    let Some(language) = gpui_kit::component::highlighter::LanguageRegistry::singleton()
        .language(lsp::language_for(path))
        .and_then(|c| c.language)
    else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else { return Vec::new() };
    let mut items = Vec::new();
    collect_declarations(tree.root_node(), text, 0, &mut items);
    items
}

/// Declarations by a part of their name: from the task's language servers,
/// which know the project; from the grammars' reading of its files where
/// no server answers.
pub(super) fn symbol_source(root: PathBuf, clients: Vec<Arc<lsp::LspClient>>, files: Arc<Vec<String>>) -> crate::finder::SymbolSource {
    use crate::finder::{DETAIL, Hit};
    const LIMIT: usize = 200;
    let declared: Arc<std::sync::OnceLock<Vec<Hit>>> = Arc::default();
    Arc::new(move |query: &str| {
        let mut hits = Vec::new();
        for client in &clients {
            for symbol in client.workspace_symbols(query).unwrap_or_default() {
                let kind = lsp::symbol_kind(symbol.kind);
                let detail = if symbol.container.is_empty() { kind.to_string() } else { format!("{kind} in {}", symbol.container) };
                hits.push(Hit {
                    path: symbol.path.strip_prefix(&root).unwrap_or(&symbol.path).to_string_lossy().into_owned(),
                    line: symbol.line + 1,
                    col: symbol.col,
                    text: format!("{}{DETAIL}{}", symbol.name, detail.trim()),
                    ranges: Vec::new(),
                });
            }
        }
        // The project's own before what its libraries declare.
        hits.sort_by_key(|hit| std::path::Path::new(&hit.path).is_absolute());
        if hits.is_empty() {
            let all = declared.get_or_init(|| {
                let mut all = Vec::new();
                for file in files.iter().take(5000) {
                    let path = root.join(file);
                    if std::fs::metadata(&path).map_or(true, |m| m.len() > 300_000) {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(&path) else { continue };
                    all.extend(outline(&path, &text).into_iter().filter_map(|item| {
                        let (kind, name) = item.label.rsplit_once(' ')?;
                        Some(Hit { path: file.clone(), line: item.line + 1, col: 0, text: format!("{name}{DETAIL}{kind}"), ranges: Vec::new() })
                    }));
                }
                all
            });
            hits = crate::finder::match_symbols(all, query, LIMIT);
        }
        hits.truncate(LIMIT);
        hits
    })
}

/// The first identifier within `depth` levels, breadth first: a Kotlin or
/// Java field names itself a level or two down (declarator, variable).
pub(super) fn first_identifier(node: tree_sitter::Node, depth: usize) -> Option<tree_sitter::Node> {
    let mut level = vec![node];
    for _ in 0..depth {
        let mut next = Vec::new();
        for n in level {
            let mut c = n.walk();
            for child in n.named_children(&mut c) {
                if child.kind().ends_with("identifier") && child.kind() != "type_identifier" {
                    return Some(child);
                }
                next.push(child);
            }
        }
        level = next;
    }
    None
}

pub(super) fn collect_declarations(node: tree_sitter::Node, text: &str, depth: usize, items: &mut Vec<crate::finder::OutlineItem>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        let declaration = ["function", "method", "class", "interface", "struct", "enum", "trait", "impl", "object", "type_spec", "type_alias", "type_item", "module", "namespace", "property", "field", "const_spec", "const_item", "static_item"]
            .iter()
            .any(|k| kind.contains(k))
            && !["call", "invocation", "reference", "expression", "parameter", "argument", "literal", "access", "body", "list", "initializer"]
                .iter()
                .any(|k| kind.contains(k))
            && !kind.ends_with("identifier")
            // A field's type or a class's body is not a declaration of its own.
            && !kind.ends_with("_type");
        let name = declaration
            .then(|| {
                child
                    .child_by_field_name("name")
                    .or_else(|| child.child_by_field_name("type"))
                    .or_else(|| first_identifier(child, 3))
            })
            .flatten()
            .and_then(|n| text.get(n.byte_range()))
            .map(|n| n.lines().next().unwrap_or(n).to_string());
        match name {
            Some(name) => {
                let short = kind
                    .trim_end_matches("_declaration")
                    .trim_end_matches("_definition")
                    .trim_end_matches("_item")
                    .trim_end_matches("_spec")
                    .replace('_', " ");
                items.push(crate::finder::OutlineItem {
                    depth,
                    label: format!("{short} {name}"),
                    line: child.start_position().row as u32,
                });
                collect_declarations(child, text, depth + 1, items);
            }
            None => collect_declarations(child, text, depth, items),
        }
    }
}

#[cfg(test)]
mod places_tests {
    use super::{Position, find_places, is_at, search_places, PlacesQuery};

    fn project(files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("ideae-places-{}-{}", std::process::id(), files.len()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        root
    }

    #[test]
    fn search_finds_implementations_and_usages_without_declarations() {
        let root = project(&[
            ("Shape.kt", "interface Shape {\n    fun area(): Double\n}\n"),
            ("Circle.kt", "class Circle(val r: Double) : Shape {\n    override fun area() = 3.14 * r * r\n}\n"),
            ("Square.java", "class Square implements Shape {}\n"),
            ("Main.kt", "fun total(shapes: List<Shape>) = shapes.sumOf { it.area() }\nval s: Shape = Circle(1.0)\n"),
        ]);
        let files = vec!["Shape.kt".into(), "Circle.kt".into(), "Square.java".into(), "Main.kt".into()];
        let (implementations, usages) = search_places(&root, &files, "Shape", true);
        let at = |hits: &[crate::finder::Hit]| hits.iter().map(|h| format!("{}:{}", h.path, h.line)).collect::<Vec<_>>();
        let mut found = at(&implementations);
        found.sort();
        assert_eq!(found, ["Circle.kt:1", "Square.java:1"]);
        let mut found = at(&usages);
        found.sort();
        assert_eq!(found, ["Main.kt:1", "Main.kt:2"]);
        assert_eq!(usages.iter().find(|h| h.line == 2).unwrap().col, 7);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_search_result_is_here_on_its_line() {
        let range = |a: (u32, u32), b: (u32, u32)| lsp_types::Range { start: Position::new(a.0, a.1), end: Position::new(b.0, b.1) };
        assert!(is_at(range((3, 0), (3, 0)), Position::new(3, 12)));
        assert!(!is_at(range((3, 0), (3, 0)), Position::new(4, 0)));
        assert!(is_at(range((3, 4), (3, 9)), Position::new(3, 9)));
        assert!(!is_at(range((3, 4), (3, 9)), Position::new(3, 10)));
    }

    /// Needs gopls: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn gopls_implementations_and_usages() {
        let root = project(&[
            ("go.mod", "module example.com/m\n\ngo 1.21\n"),
            ("shape.go", "package main\n\ntype Shape interface{ Area() float64 }\n\ntype Sq struct{}\n\nfunc (Sq) Area() float64 { return 1 }\n\nfunc main() { var s Shape = Sq{}; _ = s.Area() }\n"),
        ]);
        let file = root.join("shape.go");
        let text = std::fs::read_to_string(&file).unwrap();
        let client = crate::lsp::LspClient::spawn("gopls", std::path::Path::new("gopls"), &[], &root, serde_json::Value::Null).unwrap();
        client.did_open(&file, "go", &text).unwrap();
        let query = PlacesQuery { path: file.clone(), pos: Position::new(2, 6), word: "Shape".into(), implementations: true };
        // Ctrl+B on the declaration turns into its usages only if the server
        // answers with the declaration itself.
        let links = client.definition(&crate::lsp::path_to_uri(&file), query.pos).unwrap();
        assert!(!links.is_empty() && links.iter().all(|l| is_at(l.target_selection_range, query.pos)), "{links:?}");
        let places = find_places(&root, &[], &query, &text, Some(&client));
        assert!(!places.implementations_by_search && !places.usages_by_search);
        assert_eq!(places.implementations.iter().map(|h| h.line).collect::<Vec<_>>(), [5]);
        assert_eq!(places.usages.iter().map(|h| (h.line, h.text.as_str())).collect::<Vec<_>>(), [(9, "func main() { var s Shape = Sq{}; _ = s.Area() }")]);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod outline_tests {
    use super::outline;
    use std::path::Path;

    #[test]
    fn go_declarations() {
        let src = "package main\n\ntype Server struct{}\n\nfunc (s *Server) Run() { go s.loop() }\n\nfunc main() {}\n";
        let labels: Vec<String> = outline(Path::new("m.go"), src).into_iter().map(|i| i.label).collect();
        assert!(labels.iter().any(|l| l.ends_with("Server")), "{labels:?}");
        assert!(labels.iter().any(|l| l.ends_with("Run")), "{labels:?}");
        assert!(labels.iter().any(|l| l.ends_with("main")), "{labels:?}");
        assert!(!labels.iter().any(|l| l.contains("loop")), "{labels:?}");
    }

    #[test]
    fn fields_are_listed() {
        let ts = "class A {\n  count: number = 1;\n  name = 'x';\n  run() {}\n}\n";
        let labels: Vec<String> = outline(Path::new("a.ts"), ts).into_iter().map(|i| i.label).collect();
        assert!(labels.iter().any(|l| l.ends_with("count")), "{labels:?}");
        assert!(labels.iter().any(|l| l.ends_with("run")), "{labels:?}");
        let kt = "class A {\n    val count: Int = 1\n    fun run() {}\n}\n";
        let labels: Vec<String> = outline(Path::new("a.kt"), kt).into_iter().map(|i| i.label).collect();
        assert!(labels.iter().any(|l| l.ends_with("count")), "{labels:?}");
    }
}

/// A file name whose extension steps back: the name is what is read.
pub(super) fn file_label(name: SharedString, cx: &App) -> StyledText {
    let extension = name
        .rfind('.')
        .filter(|&dot| dot > 0)
        .map(|dot| (dot..name.len(), HighlightStyle { color: Some(cx.theme().muted_foreground), ..Default::default() }));
    StyledText::new(name).with_highlights(extension)
}

/// "Viewed" in a diff file's header, the mark the Changes list shows too.
pub(super) fn viewed_checkbox(task: &WeakEntity<TaskView>, file: &DiffFile, cx: &App) -> AnyElement {
    let path = file.path().clone();
    let viewed = task.upgrade().is_some_and(|t| t.read(cx).is_path_viewed(&path));
    let task = task.clone();
    gpui_kit::component::checkbox::Checkbox::new(SharedString::from(format!("viewed-{path}")))
        .label("Viewed")
        .checked(viewed)
        .tooltip("Viewed until it changes again")
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            let _ = task.update(cx, |t, cx| t.toggle_viewed(&path, cx));
        })
        .into_any_element()
}

/// "open" in a diff file's header: the file as it is now, at the first line
/// this diff changes in it (from `first_lines`, built from the patch shown).
pub fn open_file_button(
    root: &PathBuf,
    task: &WeakEntity<TaskView>,
    file: &DiffFile,
    first_lines: &HashMap<String, u32>,
) -> AnyElement {
    if matches!(file.status(), DiffFileStatus::Deleted) {
        return div().into_any_element();
    }
    let path = root.join(file.path().as_ref());
    let task = task.clone();
    let key = file.path().clone();
    let line = first_lines.get(key.as_ref()).copied().unwrap_or(1).saturating_sub(1);
    Button::new(SharedString::from(format!("open-{key}")))
        .xsmall()
        .ghost()
        .label("open")
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            let path = path.clone();
            let _ = task.update(cx, |t, cx| t.open_file(path, line, window, cx));
        })
        .into_any_element()
}

/// Where `symbol` is declared: a project file of that name; a class's file
/// (then `Class.member` by its outline); a class declaration; a function or
/// variable declaration. 0-based lines, with the line's code for display.
pub fn find_symbol(root: &std::path::Path, files: &[String], symbol: &str) -> Vec<(PathBuf, u32, String)> {
    let file_name_of = |f: &str| f.rsplit('/').next().unwrap_or(f).to_string();
    if let Some(file) = files.iter().find(|f| file_name_of(f) == symbol) {
        return vec![(root.join(file), 0, String::new())];
    }
    let rg = |patterns: &[String], max: u64| -> Vec<(PathBuf, u32, String)> {
        let Some(matcher) = crate::search::regexes(patterns) else { return Vec::new() };
        crate::search::search(root, &matcher, max, 200, 50)
            .into_iter()
            .map(|m| (root.join(m.path), m.line.saturating_sub(1) as u32, m.text))
            .collect()
    };
    let lowercase = symbol.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    if lowercase && !symbol.contains('.') {
        return rg(&function_declarations(symbol), 5);
    }
    let (class, member) = symbol.split_once('.').map_or((symbol, None), |(c, m)| (c, Some(m)));
    let found = files
        .iter()
        .find(|f| file_name_of(f).split('.').next() == Some(class) && f.contains('.'))
        .map(|f| (root.join(f), 0u32, String::new()))
        .or_else(|| rg(&type_declarations(class), 1).into_iter().next());
    let Some((path, mut line, code)) = found else { return Vec::new() };
    if let Some(member) = member {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(item) = outline(&path, &text).into_iter().find(|i| i.label.rsplit(' ').next() == Some(member)) {
                line = item.line;
            }
        }
    }
    vec![(path, line, code)]
}

/// Lines that declare a function or variable `name`, for ripgrep.
pub(super) fn function_declarations(name: &str) -> Vec<String> {
    vec![
        format!(r"\b(fun|func|function|def|fn|val|var|const|let)\s+([A-Za-z_][\w.]*\.)?{name}\b"),
        // Go methods: func (r *T) name(
        format!(r"\bfunc\s*\([^)]*\)\s*{name}\b"),
        // Java/C-style: a type then the name then "(" at the start of a line.
        format!(r"^\s*(public|private|protected|static|final|override|suspend|internal|\s)*[\w<>\[\],.? ]+\s+{name}\s*\("),
    ]
}

/// Lines that declare a type `name`, for ripgrep.
pub(super) fn type_declarations(name: &str) -> Vec<String> {
    vec![format!(r"\b(class|object|interface|struct|enum|trait|type)\s+{name}\b")]
}

/// Lines that implement or extend a type `name`, for ripgrep.
pub(super) fn implementing_declarations(name: &str) -> Vec<String> {
    vec![
        // Rust: impl<T> Name<T> for Type
        format!(r"\bimpl\b[^{{;]*\b{name}\b[^{{;]*\bfor\b"),
        // Java, TypeScript, PHP: class A extends B implements Name
        format!(r"\b(implements|extends)\b[^{{]*\b{name}\b"),
        // Kotlin, Swift, C#, C++: class A : Name, after a name or a constructor.
        format!(r"\b(class|object|struct|enum|interface|record)\s+\w+\s*(<[^>]*>)?\s*:[^{{=]*\b{name}\b"),
        format!(r"\b(class|object)\s+\w+.*\)\s*:[^{{=(]*\b{name}\b"),
        // Python: class A(Name)
        format!(r"^\s*class\s+\w+\s*\([^)]*\b{name}\b"),
        // Go: a struct does not name its interface, so nothing to find there.
    ]
}

/// What Ctrl+Alt+B or Ctrl+B on a declaration asks for.
pub(super) struct PlacesQuery {
    pub(super) path: PathBuf,
    pub(super) pos: Position,
    pub(super) word: String,
    pub(super) implementations: bool,
}

/// The answer, each list sorted by file and line.
#[derive(Default)]
pub(super) struct Places {
    pub(super) implementations: Vec<crate::finder::Hit>,
    pub(super) usages: Vec<crate::finder::Hit>,
    pub(super) implementations_by_search: bool,
    pub(super) usages_by_search: bool,
}

/// Off the UI thread: the language server where it answers, text search
/// where it cannot (none, not started, no such request, an error). `buffer`
/// is the open file's text, which may differ from the disk.
pub(super) fn find_places(
    root: &std::path::Path,
    files: &[String],
    query: &PlacesQuery,
    buffer: &str,
    client: Option<&lsp::LspClient>,
) -> Places {
    let uri = lsp::path_to_uri(&query.path);
    let ask = |provider: &str, request: &dyn Fn() -> anyhow::Result<Vec<lsp_types::Location>>| {
        let client = client?;
        if !client.supports(provider) {
            return None;
        }
        request().inspect_err(|e| eprintln!("{provider}: {e:#}")).ok()
    };
    let (implementations, usages) = std::thread::scope(|s| {
        let implementations = s.spawn(|| {
            if !query.implementations {
                return Some(Vec::new());
            }
            ask("implementationProvider", &|| client.unwrap().implementation(&uri, query.pos))
        });
        let usages = ask("referencesProvider", &|| client.unwrap().references(&uri, query.pos));
        (implementations.join().unwrap_or(None), usages)
    });

    let mut places = Places::default();
    let mut texts = TextCache::new(&query.path, buffer);
    let mut search = None;
    let mut by_search = || search.get_or_insert_with(|| search_places(root, files, &query.word, query.implementations)).clone();
    match implementations {
        Some(found) => places.implementations = found.iter().filter_map(|l| texts.hit(root, l)).collect(),
        None => {
            places.implementations = by_search().0;
            places.implementations_by_search = true;
        }
    }
    match usages {
        Some(found) => places.usages = found.iter().filter_map(|l| texts.hit(root, l)).collect(),
        None => {
            places.usages = by_search().1;
            places.usages_by_search = true;
        }
    }
    for list in [&mut places.implementations, &mut places.usages] {
        list.sort_by(|a, b| (&a.path, a.line, a.col).cmp(&(&b.path, b.line, b.col)));
        list.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.col == b.col);
    }
    // The place asked about is no answer, even when the server lists it.
    let origin = (query.path.strip_prefix(root).unwrap_or(&query.path).display().to_string(), query.pos.line + 1);
    places.implementations.retain(|h| (h.path.clone(), h.line) != origin);
    places.usages.retain(|h| (h.path.clone(), h.line) != origin);
    // Some servers list an implementation among the references too.
    let implemented: HashSet<(String, u32)> = places.implementations.iter().map(|h| (h.path.clone(), h.line)).collect();
    places.usages.retain(|h| !implemented.contains(&(h.path.clone(), h.line)));
    places
}

/// Lines of the files locations point into, read once each.
pub(super) struct TextCache {
    pub(super) texts: HashMap<PathBuf, Option<String>>,
}

impl TextCache {
    pub(super) fn new(open: &std::path::Path, buffer: &str) -> Self {
        Self { texts: HashMap::from([(open.to_path_buf(), Some(buffer.to_string()))]) }
    }

    /// A location as a place in the list: its path relative to `root` when
    /// inside it, its line's code.
    pub(super) fn hit(&mut self, root: &std::path::Path, location: &lsp_types::Location) -> Option<crate::finder::Hit> {
        let path = lsp::uri_to_path(location.uri.as_str())?;
        let start = location.range.start;
        let text = self
            .texts
            .entry(path.clone())
            .or_insert_with(|| std::fs::read_to_string(&path).ok())
            .as_deref()
            .and_then(|t| t.lines().nth(start.line as usize))
            .map(|l| l.trim().chars().take(300).collect())
            .unwrap_or_default();
        let shown = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        Some(crate::finder::Hit { path: shown, line: start.line + 1, col: start.character, text, ranges: Vec::new() })
    }
}

/// Without a language server: implementations by the declarations that name
/// the type, usages as every whole-word occurrence that is no declaration.
pub(super) fn search_places(
    root: &std::path::Path,
    files: &[String],
    word: &str,
    implementations: bool,
) -> (Vec<crate::finder::Hit>, Vec<crate::finder::Hit>) {
    let rg = |patterns: &[String], per_file: u64, limit: usize| -> Vec<crate::finder::Hit> {
        let Some(matcher) = crate::search::regexes(patterns) else { return Vec::new() };
        crate::search::search(root, &matcher, per_file, 300, limit)
            .into_iter()
            .map(|m| {
                let col = m.text.find(word).map_or(0, |b| m.text[..b].chars().count() as u32);
                crate::finder::Hit { path: m.path.display().to_string(), line: m.line as u32, col, text: m.text.trim().to_string(), ranges: Vec::new() }
            })
            .collect()
    };
    let is_type = word.chars().next().is_some_and(|c| c.is_uppercase());
    let found_implementations = if implementations && is_type { rg(&implementing_declarations(word), 20, 200) } else { Vec::new() };
    let mut declarations: Vec<String> = function_declarations(word);
    declarations.extend(type_declarations(word));
    let declared: HashSet<(String, u32)> = rg(&declarations, 20, 200)
        .into_iter()
        .chain(find_symbol(root, files, word).into_iter().map(|(path, line, _)| crate::finder::Hit {
            path: path.strip_prefix(root).unwrap_or(&path).display().to_string(),
            line: line + 1,
            col: 0,
            text: String::new(),
            ranges: Vec::new(),
        }))
        .chain(found_implementations.iter().cloned())
        .map(|h| (h.path, h.line))
        .collect();
    let usages = rg(&[format!(r"\b{word}\b")], 100, 1000)
        .into_iter()
        .filter(|h| !declared.contains(&(h.path.clone(), h.line)))
        .collect();
    (found_implementations, usages)
}

/// The location `UsagesAtDeclaration` hands the editor for "show usages".
pub(super) const USAGES_URI: &str = "ideae-usages:here";

/// Ctrl+B and Ctrl+click on a declaration itself list its usages, as in
/// IntelliJ, rather than jump to where the cursor already is. The editor
/// follows whatever location it is given, so the request travels as a
/// location of its own (`USAGES_URI`), which `show_document_handler` takes.
pub(super) struct UsagesAtDeclaration {
    pub(super) inner: Rc<dyn DefinitionProvider>,
    pub(super) path: PathBuf,
}

impl UsagesAtDeclaration {
    pub(super) fn wrap(inner: Rc<dyn DefinitionProvider>, path: &std::path::Path) -> Rc<dyn DefinitionProvider> {
        Rc::new(Self { inner, path: path.to_path_buf() })
    }
}

/// Whether `target` is where `pos` is: inside the range, or on its line for
/// a search result, which has a line only.
pub(super) fn is_at(target: lsp_types::Range, pos: Position) -> bool {
    if target.start == target.end {
        return target.start.line == pos.line;
    }
    let key = |p: Position| (p.line, p.character);
    key(target.start) <= key(pos) && key(pos) <= key(target.end)
}

impl DefinitionProvider for UsagesAtDeclaration {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<lsp_types::LocationLink>>> {
        let pos = text.offset_to_position(offset);
        let found = self.inner.definitions(text, offset, window, cx);
        let path = self.path.clone();
        cx.spawn(async move |_| {
            let links = found.await?;
            let here = |l: &lsp_types::LocationLink| {
                lsp::uri_to_path(l.target_uri.as_str()).as_deref() == Some(path.as_path()) && is_at(l.target_selection_range, pos)
            };
            if links.is_empty() || !links.iter().all(here) {
                return Ok(links);
            }
            let at = lsp_types::Range { start: pos, end: pos };
            Ok(vec![lsp_types::LocationLink {
                origin_selection_range: links[0].origin_selection_range,
                target_uri: USAGES_URI.parse()?,
                target_range: at,
                target_selection_range: at,
            }])
        })
    }
}

/// Ctrl+click and Ctrl+B in the editor when no language server runs: the
/// word under the cursor (with `Class.` before it, if any) by `find_symbol`.
pub(super) struct SearchDefinitionProvider {
    pub(super) root: PathBuf,
    pub(super) files: Arc<std::sync::RwLock<Arc<Vec<String>>>>,
}

impl DefinitionProvider for SearchDefinitionProvider {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<lsp_types::LocationLink>>> {
        let Some(range) = text.word_range(offset) else { return Task::ready(Ok(Vec::new())) };
        let word = text.slice(range.clone()).to_string();
        // `Model.silentFail`: the member of that class, not any silentFail.
        let before = text.slice(range.start.saturating_sub(64)..range.start).to_string();
        let symbol = match before.strip_suffix('.') {
            Some(prefix) => {
                let class: String = prefix.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<Vec<_>>().into_iter().rev().collect();
                if class.chars().next().is_some_and(|c| c.is_ascii_uppercase()) { format!("{class}.{word}") } else { word.clone() }
            }
            None => word.clone(),
        };
        let origin = lsp_types::Range { start: text.offset_to_position(range.start), end: text.offset_to_position(range.end) };
        let (root, files) = (self.root.clone(), self.files.clone());
        cx.background_spawn(async move {
            let files = files.read().unwrap().clone();
            let links = find_symbol(&root, &files, &symbol)
                .into_iter()
                .filter_map(|(path, line, _)| {
                    let at = lsp_types::Range {
                        start: lsp_types::Position::new(line, 0),
                        end: lsp_types::Position::new(line, 0),
                    };
                    Some(lsp_types::LocationLink {
                        origin_selection_range: Some(origin),
                        target_uri: lsp::path_to_uri(&path).parse().ok()?,
                        target_range: at,
                        target_selection_range: at,
                    })
                })
                .collect();
            Ok(links)
        })
    }
}

/// Definitions in other files open as tabs; same-file ones fall through to
/// the editor's own cursor move.
pub(super) fn show_document_handler(task: WeakEntity<TaskView>, this_path: PathBuf) -> gpui_kit::component::input::ShowDocumentHandler {
    Rc::new(move |params, window, cx| {
        if params.uri.as_str() == USAGES_URI {
            let pos = params.selection.map(|r| r.start).unwrap_or_default();
            let (path, task) = (this_path.clone(), task.clone());
            // Called while the editor is being updated, and the list reads it.
            window.defer(cx, move |window, cx| {
                let _ = task.update(cx, |task, cx| task.show_places(path, pos, false, window, cx));
            });
            return true;
        }
        let Some(target) = lsp::uri_to_path(params.uri.as_str()) else {
            return false;
        };
        if target == this_path {
            // The editor moves the cursor itself; this is where it was.
            let _ = task.update(cx, |task, cx| task.record_nav(cx));
            return false;
        }
        let line = params.selection.map(|r| r.start.line).unwrap_or(0);
        task.update(cx, |task, cx| task.open_file(target, line, window, cx)).is_ok()
    })
}
