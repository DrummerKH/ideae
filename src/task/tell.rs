//! What a task tells its own agent through ideae's MCP server (`crate::mcp`):
//! its open files and selection, its language servers' problems. Only this
//! task's: the workspace asks the task the question's worktree names.

use super::*;
use std::path::Path;

/// The most a selection or a list of problems gives, so an answer stays a
/// size an agent reads.
const SELECTION_MOST: usize = 8 << 10;
const PROBLEMS_MOST: usize = 200;

impl TaskView {
    /// The files open in the editor: the one in front (last used) with its
    /// cursor and selection, those with edits not saved.
    pub fn mcp_open_files(&self, cx: &App) -> String {
        let mut files: Vec<(u64, &Entity<FileView>)> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::File(f) => Some((t.used, f)),
                _ => None,
            })
            .collect();
        if files.is_empty() {
            return "No file of this task is open in the editor.".into();
        }
        files.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
        let rel = |path: &Path| path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
        let mut out = String::new();
        let (_, front) = files[0];
        {
            let f = front.read(cx);
            let state = f.editor.read(cx);
            let range = state.selected_range();
            let line = state.cursor_position().line + 1;
            out.push_str(&format!("In front: {} (cursor on line {line}){}\n", rel(&f.path), if f.dirty { ", not saved" } else { "" }));
            if !range.is_empty() {
                // Its start alone: a whole file selected is not copied for it.
                let cut = range.len() > SELECTION_MOST;
                let mut selected = String::new();
                for c in state.text().slice(range).chars() {
                    if selected.len() + c.len_utf8() > SELECTION_MOST {
                        break;
                    }
                    selected.push(c);
                }
                out.push_str(&format!("Selected:\n```\n{selected}\n```\n"));
                if cut {
                    out.push_str("(the selection goes on)\n");
                }
            }
        }
        if files.len() > 1 {
            out.push_str("Also open:\n");
            for (_, f) in &files[1..] {
                let f = f.read(cx);
                out.push_str(&format!("- {}{}\n", rel(&f.path), if f.dirty { " (not saved)" } else { "" }));
            }
        }
        out
    }

    /// The problems the language servers report: of `path` (in this
    /// worktree), or of every open file.
    pub fn mcp_diagnostics(&self, path: Option<&str>, cx: &App) -> Result<String, String> {
        let wanted = match path.filter(|p| !p.trim().is_empty()) {
            Some(p) => {
                let p = Path::new(p);
                let full = if p.is_absolute() { p.to_path_buf() } else { self.root.join(p) };
                if !crate::acp::policy::inside(&self.root, &full) {
                    return Err(format!("{} is not in this task's worktree", full.display()));
                }
                Some(full)
            }
            None => None,
        };
        // Each open file's own server, else any of the task's for a file not open.
        let mut asked: Vec<(PathBuf, Arc<lsp::LspClient>)> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::File(f) => {
                    let f = f.read(cx);
                    f.lsp_client.clone().map(|c| (f.path.clone(), c))
                }
                _ => None,
            })
            .filter(|(p, _)| wanted.as_ref().is_none_or(|w| w == p))
            .collect();
        if let (Some(w), true) = (&wanted, asked.is_empty()) {
            asked.extend(self.lsp.clients().into_iter().map(|c| (w.clone(), c)));
        }
        if asked.is_empty() {
            return Ok(match wanted {
                Some(w) => format!("No language server looks at {}: open it in ideae to have it analysed.", w.strip_prefix(&self.root).unwrap_or(&w).display()),
                None => "No open file of this task has a language server.".into(),
            });
        }
        let mut lines = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (path, client) in asked {
            let (_, problems) = client.problems(&lsp::path_to_uri(&path));
            let rel = path.strip_prefix(&self.root).unwrap_or(&path).display().to_string();
            for p in problems {
                let severity = match p.severity {
                    Some(lsp_types::DiagnosticSeverity::ERROR) => "error",
                    Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
                    Some(lsp_types::DiagnosticSeverity::INFORMATION) => "info",
                    _ => "hint",
                };
                let line = format!("{rel}:{}:{}: {severity}: {}", p.range.start.line + 1, p.range.start.character + 1, p.message.lines().next().unwrap_or_default());
                if seen.insert(line.clone()) {
                    lines.push(line);
                }
            }
        }
        if lines.is_empty() {
            return Ok("No problems reported.".into());
        }
        let more = lines.len().saturating_sub(PROBLEMS_MOST);
        lines.truncate(PROBLEMS_MOST);
        if more > 0 {
            lines.push(format!("and {more} more"));
        }
        Ok(lines.join("\n"))
    }

    /// When the last check started, to tell the next one from it.
    pub fn mcp_check_started(&self) -> Option<u64> {
        self.auto.check.as_ref().map(|c| c.at)
    }

    /// The check's result: None while it runs, or while the one there is
    /// the run before (`before`).
    pub fn mcp_check_done(&self, before: Option<u64>) -> Option<String> {
        let check = self.auto.check.as_ref().filter(|c| Some(c.at) != before)?;
        let state = match check.state {
            auto::RunState::Running => return None,
            auto::RunState::Passed => "passed".to_string(),
            auto::RunState::Failed(Some(code)) => format!("failed (exit code {code})"),
            auto::RunState::Failed(None) => "failed".to_string(),
            auto::RunState::Stopped => "was stopped".to_string(),
        };
        let lines: Vec<&str> = check.output.lines().collect();
        let tail = lines[lines.len().saturating_sub(80)..].join("\n");
        Some(format!("`{}` {state}.\n```\n{tail}\n```", check.command))
    }

    /// Whether its agent asked a language server within `idle`: the task's
    /// servers are not put to sleep under it.
    pub fn asked_of_lately(&self, idle: Duration) -> bool {
        self.lsp_asked.is_some_and(|at| at.elapsed() < idle)
    }

    /// What an agent asks of the task's language servers (`definition`,
    /// `references`, `hover`, `symbols`): the server found or started here,
    /// the asking done by [`CodeQuestion::answer`] off this thread.
    pub fn mcp_code(&mut self, tool: &str, args: &serde_json::Value) -> Result<CodeQuestion, String> {
        let path = args["path"].as_str().map(str::trim).filter(|p| !p.is_empty());
        let full = match path {
            Some(p) => {
                let p = Path::new(p);
                let full = if p.is_absolute() { p.to_path_buf() } else { self.root.join(p) };
                if !crate::acp::policy::inside(&self.root, &full) {
                    return Err(format!("{} is not in this task's worktree", full.display()));
                }
                Some(full)
            }
            None => None,
        };
        self.lsp_asked = Some(std::time::Instant::now());
        if tool == "symbols" {
            let query = args["query"].as_str().unwrap_or_default().trim().to_string();
            if query.is_empty() {
                return Err("Say what to look for: a name or the start of one.".into());
            }
            return Ok(CodeQuestion { root: self.root.clone(), tool: Tool::Symbols(query), clients: Vec::new(), servers: Some(self.servers_asked()), start: full });
        }
        let full = full.ok_or("Give `path`, the file the name is in.")?;
        let line = args["line"].as_u64().filter(|l| *l > 0).ok_or("Give `line`, counted from 1.")? as usize;
        let at = match (args["column"].as_u64().filter(|c| *c > 0), args["symbol"].as_str().filter(|s| !s.is_empty())) {
            (Some(column), _) => At::Column(column as usize - 1),
            (None, Some(symbol)) => At::Symbol(symbol.to_string()),
            (None, None) => return Err("Give `symbol`, the name as written on the line, or `column`, counted from 1.".into()),
        };
        let tool = match tool {
            "definition" => Tool::Definition,
            "references" => Tool::References,
            _ => Tool::Hover,
        };
        let place = Place { path: full, line, at, language_id: "" };
        Ok(CodeQuestion { root: self.root.clone(), tool: Tool::Place(Box::new(tool), place), clients: Vec::new(), servers: Some(self.servers_asked()), start: None })
    }

    /// The task's servers for a question, found or started off the UI
    /// thread; none when its project has them off.
    fn servers_asked(&self) -> Asked {
        Asked { servers: self.lsp.clone(), off: self.lsp_off }
    }
}

/// A question for the language servers, asked off the UI thread.
pub struct CodeQuestion {
    root: PathBuf,
    tool: Tool,
    /// The servers asked: given, or found by `servers` for the file asked
    /// about (`start`, the file whose language to search for symbols).
    clients: Vec<Arc<lsp::LspClient>>,
    servers: Option<Asked>,
    start: Option<PathBuf>,
}

struct Asked {
    servers: Arc<crate::lsp_share::TaskServers>,
    off: bool,
}

impl Asked {
    fn server_for(&self, root: &Path, path: &Path) -> Result<crate::lsp_share::Found, String> {
        if self.off {
            return Err(super::open::LSP_OFF.into());
        }
        self.servers.server_for(root, path)
    }
}

enum Tool {
    Definition,
    References,
    Hover,
    Symbols(String),
    /// One of the first three, of a place in a file.
    Place(Box<Tool>, Place),
}

struct Place {
    path: PathBuf,
    line: usize,
    at: At,
    language_id: &'static str,
}

enum At {
    Column(usize),
    Symbol(String),
}

/// How long an answer waits for a server still reading the project.
const READING_MOST: Duration = Duration::from_secs(60);
const PLACES_MOST: usize = 100;

impl CodeQuestion {
    /// Asks, waiting for a server that is still reading the project rather
    /// than telling an agent nothing is there. Blocks: finds or starts the
    /// server first.
    pub fn answer(mut self) -> Result<String, String> {
        if let Some(asked) = &self.servers {
            match &mut self.tool {
                Tool::Place(_, place) => {
                    let found = asked.server_for(&self.root, &place.path)?;
                    place.language_id = found.language_id;
                    self.clients = vec![found.client];
                }
                _ => {
                    if let Some(start) = &self.start {
                        asked.server_for(&self.root, start)?;
                    }
                    self.clients = asked.servers.clients();
                    if self.clients.is_empty() {
                        return Err("No language server runs for this task yet: give `path`, a file of the language to search.".into());
                    }
                }
            }
        }
        let rel = |path: &Path| path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
        match &self.tool {
            Tool::Symbols(query) => {
                let mut lines = Vec::new();
                for client in &self.clients {
                    for symbol in client.workspace_symbols(query).map_err(|e| e.to_string())? {
                        if client.foreign_to_root(&self.root, &symbol.path) {
                            continue;
                        }
                        let kind = lsp::symbol_kind(symbol.kind);
                        let within = if symbol.container.is_empty() { String::new() } else { format!(" in {}", symbol.container) };
                        lines.push(format!("{} ({kind}{within}): {}:{}", symbol.name, rel(&symbol.path), symbol.line + 1));
                    }
                }
                lines.truncate(PLACES_MOST);
                Ok(if lines.is_empty() { format!("Nothing declared like `{query}`.") } else { lines.join("\n") })
            }
            Tool::Place(tool, place) => {
                let client = &self.clients[0];
                let text = std::fs::read_to_string(&place.path).map_err(|e| format!("{}: {e}", rel(&place.path)))?;
                let line_text = text.lines().nth(place.line - 1).ok_or_else(|| format!("{} has no line {}", rel(&place.path), place.line))?;
                let character = match &place.at {
                    At::Column(column) => *column,
                    At::Symbol(symbol) => {
                        let byte = line_text.find(symbol.as_str()).ok_or_else(|| format!("`{symbol}` is not on line {} of {}", place.line, rel(&place.path)))?;
                        line_text[..byte].chars().count()
                    }
                };
                let pos = lsp_types::Position { line: place.line as u32 - 1, character: character as u32 };
                let uri = lsp::path_to_uri(&place.path);
                // Opened for the question alone when the editor has it not: then closed, the server reads the disk again.
                let ours = client.borrow_file(&place.path, place.language_id, &text).map_err(|e| e.to_string())?;
                let start = std::time::Instant::now();
                let answer = loop {
                    let found = match tool.as_ref() {
                        Tool::Definition => client.definition(&uri, pos).map(|links| {
                            links.into_iter().map(|l| (l.target_uri.to_string(), l.target_selection_range.start)).collect::<Vec<_>>()
                        }),
                        Tool::References => client.references(&uri, pos).map(|found| found.into_iter().map(|l| (l.uri.to_string(), l.range.start)).collect()),
                        _ => client.hover(&uri, pos).map(|hover| match hover {
                            Some(hover) => vec![(hover_text(&hover), pos)],
                            None => Vec::new(),
                        }),
                    };
                    match found {
                        Ok(found) if !found.is_empty() || !client.reading(&place.path) || start.elapsed() > READING_MOST => break Ok((found, client.reading(&place.path))),
                        // "content modified": asked while the server took in a change, to be asked again.
                        Err(e) if (!client.reading(&place.path) && !e.to_string().contains("content modified")) || start.elapsed() > READING_MOST => {
                            break Err(e.to_string());
                        }
                        _ => std::thread::sleep(Duration::from_secs(1)),
                    }
                };
                if ours {
                    client.return_file(&place.path);
                }
                let (found, reading) = answer?;
                if found.is_empty() {
                    return Ok(if reading {
                        "The language server is still reading the project (indexing): ask again in a minute.".into()
                    } else {
                        "Nothing found there.".into()
                    });
                }
                if matches!(tool.as_ref(), Tool::Hover) {
                    return Ok(found.into_iter().map(|(text, _)| text).collect::<Vec<_>>().join("\n"));
                }
                let mut lines: Vec<String> = found
                    .into_iter()
                    .take(PLACES_MOST)
                    .filter_map(|(uri, at)| {
                        let path = lsp::uri_to_path(&uri)?;
                        let said = std::fs::read_to_string(&path).ok().and_then(|t| t.lines().nth(at.line as usize).map(|l| l.trim().to_string())).unwrap_or_default();
                        Some(format!("{}:{}:{}: {said}", rel(&path), at.line + 1, at.character + 1))
                    })
                    .collect();
                lines.dedup();
                Ok(lines.join("\n"))
            }
            _ => Err("not a question for a place".into()),
        }
    }
}

fn hover_text(hover: &lsp_types::Hover) -> String {
    use lsp_types::{HoverContents, MarkedString};
    let marked = |m: &MarkedString| match m {
        MarkedString::String(s) => s.clone(),
        MarkedString::LanguageString(l) => format!("```{}\n{}\n```", l.language, l.value),
    };
    match &hover.contents {
        HoverContents::Scalar(m) => marked(m),
        HoverContents::Array(all) => all.iter().map(marked).collect::<Vec<_>>().join("\n\n"),
        HoverContents::Markup(m) => m.value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{At, CodeQuestion, Place, Tool};
    use crate::lsp;

    /// An agent's questions of a rust-analyzer that has just started: asked
    /// while it reads the project, answered once it has. Passes without
    /// running where there is no rust-analyzer.
    #[test]
    #[ignore]
    fn an_agent_asks_the_language_server() {
        let root = std::env::temp_dir().join(format!("ideae-ask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        let lib = root.join("src/lib.rs");
        std::fs::write(&lib, "/// The answer.\npub fn answer() -> u32 {\n    42\n}\n\npub fn ask() -> u32 {\n    answer()\n}\n\npub fn again() -> u32 {\n    answer() + 1\n}\n").unwrap();
        let Ok(server) = lsp::server_for(&lib) else {
            eprintln!("no rust-analyzer here: skipped");
            return;
        };
        let client = lsp::LspClient::spawn(server.program, &server.command, server.args, &root, serde_json::Value::Null).unwrap();
        let ask = |tool: Tool, line: usize, symbol: &str| {
            let place = Place { path: lib.clone(), line, at: At::Symbol(symbol.into()), language_id: "rust" };
            CodeQuestion { root: root.clone(), tool: Tool::Place(Box::new(tool), place), clients: vec![client.clone()], servers: None, start: None }.answer().unwrap()
        };
        assert_eq!(ask(Tool::Definition, 7, "answer"), "src/lib.rs:2:8: pub fn answer() -> u32 {");
        assert_eq!(ask(Tool::References, 2, "answer"), "src/lib.rs:7:5: answer()\nsrc/lib.rs:11:5: answer() + 1");
        let hover = ask(Tool::Hover, 7, "answer");
        assert!(hover.contains("pub fn answer() -> u32") && hover.contains("The answer."), "{hover}");
        let symbols = CodeQuestion { root: root.clone(), tool: Tool::Symbols("answ".into()), clients: vec![client.clone()], servers: None, start: None }.answer().unwrap();
        assert!(symbols.starts_with("answer (function"), "{symbols}");
        assert!(symbols.contains("src/lib.rs:2"), "{symbols}");
        // Asked of the file, not left open in the server.
        assert!(!client.has_open(&lib));
        // Opened by the editor while a question has it: the editor's from then on.
        assert!(client.borrow_file(&lib, "rust", "").unwrap());
        client.did_open(&lib, "rust", &std::fs::read_to_string(&lib).unwrap()).unwrap();
        client.return_file(&lib);
        assert!(client.has_open(&lib));
        let wrong = CodeQuestion {
            root: root.clone(),
            tool: Tool::Place(Box::new(Tool::Definition), Place { path: lib.clone(), line: 7, at: At::Symbol("nope".into()), language_id: "rust" }),
            clients: vec![client.clone()],
            servers: None,
            start: None,
        }
        .answer();
        assert_eq!(wrong, Err("`nope` is not on line 7 of src/lib.rs".into()));
        client.shut_down();
        let _ = std::fs::remove_dir_all(&root);
    }
}
