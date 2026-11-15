//! What a task's Claude Code session has used, from its transcript in
//! `~/.claude/projects`: shown when the pointer rests on the agent's tab.

use std::{
    collections::HashMap,
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
};

use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::*;
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub model: String,
    /// What the latest answer had in context: its input, cached or not.
    pub context: u64,
    /// The whole session.
    pub input: u64,
    pub cache_read: u64,
    pub output: u64,
    pub answers: u32,
}

/// Claude Code keeps a project's sessions in a folder named after its path,
/// every character but letters and digits made a dash.
fn sessions_dir(root: &Path) -> Option<PathBuf> {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| PathBuf::from(h).join(".claude")))?;
    let name: String = root.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    Some(config.join("projects").join(name))
}

/// The task's latest session: the transcript written to last.
pub fn transcript(root: &Path) -> Option<PathBuf> {
    std::fs::read_dir(sessions_dir(root)?)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(at, _)| *at)
        .map(|(_, path)| path)
}

pub fn read(path: &Path) -> Option<Usage> {
    let file = std::fs::File::open(path).ok()?;
    // A message is written again as it grows: the last copy of each counts.
    let mut answers: HashMap<String, (u64, u64, u64)> = HashMap::new();
    let mut usage = Usage::default();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let message = &v["message"];
        let u = &message["usage"];
        if v["type"] != "assistant" || !u.is_object() {
            continue;
        }
        let n = |key: &str| u[key].as_u64().unwrap_or(0);
        let (input, created, read, output) =
            (n("input_tokens"), n("cache_creation_input_tokens"), n("cache_read_input_tokens"), n("output_tokens"));
        let id = message["id"].as_str().unwrap_or_default().to_string();
        answers.insert(id, (input + created, read, output));
        usage.context = input + created + read;
        if let Some(model) = message["model"].as_str().filter(|m| !m.starts_with('<')) {
            usage.model = model.to_string();
        }
    }
    for (input, read, output) in answers.values() {
        usage.input += input;
        usage.cache_read += read;
        usage.output += output;
    }
    usage.answers = answers.len() as u32;
    Some(usage)
}

fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.0}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// The tooltip: reads the transcript off the UI thread.
pub struct UsageTooltip {
    usage: Option<Option<Usage>>,
    _read: Task<()>,
}

impl UsageTooltip {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let read = cx.spawn(async move |this, cx| {
            let usage = cx.background_spawn(async move { transcript(&root).and_then(|p| read(&p)) }).await;
            let _ = this.update(cx, |t, cx| {
                t.usage = Some(usage);
                cx.notify();
            });
        });
        Self { usage: None, _read: read }
    }
}

impl Render for UsageTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let row = |name: &str, value: String| {
            gpui_kit::component::h_flex()
                .gap_3()
                .child(div().w(px(110.)).text_color(muted).child(name.to_string()))
                .child(value)
        };
        let body = match &self.usage {
            None => v_flex().child("Reading the session…"),
            Some(None) => v_flex().child("No Claude Code session here yet."),
            Some(Some(u)) => v_flex()
                .gap_0p5()
                .child(row("Model", if u.model.is_empty() { "?".into() } else { u.model.clone() }))
                .child(row("In context", format!("{} tokens", tokens(u.context))))
                .child(row("Answers", u.answers.to_string()))
                .child(row("Input", format!("{} (+{} from cache)", tokens(u.input), tokens(u.cache_read))))
                .child(row("Output", tokens(u.output))),
        };
        div()
            .p_2()
            .text_xs()
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .shadow_md()
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{read, sessions_dir};

    #[test]
    fn counts_each_answer_once_and_keeps_the_latest_context() {
        let dir = std::env::temp_dir().join(format!("ideae-usage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let answer = |id: &str, input: u64, read: u64, output: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"{id}","model":"claude-x","usage":{{"input_tokens":{input},"cache_creation_input_tokens":0,"cache_read_input_tokens":{read},"output_tokens":{output}}}}}}}"#
            )
        };
        let lines = [answer("a", 10, 100, 1), answer("a", 10, 100, 5), r#"{"type":"user","message":{}}"#.into(), answer("b", 20, 300, 7)];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let u = read(&path).unwrap();
        assert_eq!((u.answers, u.input, u.cache_read, u.output, u.context), (2, 30, 400, 12, 320));
        assert_eq!(u.model, "claude-x");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sessions_dir_dashes_the_path() {
        let dir = sessions_dir(Path::new("/ott/a.worktrees/b_c")).unwrap();
        assert!(dir.ends_with("projects/-ott-a-worktrees-b-c"), "{dir:?}");
    }
}
