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
            asked.extend(self.lsp.values().map(|c| (w.clone(), c.clone())));
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
}
