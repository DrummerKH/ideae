//! What the agent said: the last words of its turn, or what it waits on,
//! made plain and short for the inbox, a row's tooltip and a notification.
//! Read by `ideae --hook` (off the window), shortened without a model.

use std::{
    io::{Read as _, Seek as _, SeekFrom},
    path::Path,
};

use serde_json::Value;

/// What crosses the socket and stays with a task: the tooltip shows it whole.
const KEPT_CHARS: usize = 800;
const KEPT_LINES: usize = 14;
/// Enough for the last answer of a long turn; the rest of the file is older.
const TAIL_BYTES: u64 = 512 * 1024;

/// The last words of a turn, from what a Stop hook is given: the answer
/// itself in newer Claude Code, the end of the transcript in older.
pub fn last_words(payload: &Value) -> Option<String> {
    let text = match payload["last_assistant_message"].as_str().filter(|s| !s.trim().is_empty()) {
        Some(text) => text.to_string(),
        None => answer_in_tail(Path::new(payload["transcript_path"].as_str()?))?,
    };
    Some(plain(&text)).filter(|s| !s.is_empty())
}

/// What a tool call waits for, when its words say more than the activity:
/// the question asked, the plan given, the command to run.
pub fn asks(payload: &Value) -> Option<String> {
    let input = &payload["tool_input"];
    let text = match payload["tool_name"].as_str()? {
        "AskUserQuestion" => {
            let questions = input["questions"].as_array()?;
            let first = questions.first()?["question"].as_str()?.trim().to_string();
            match questions.len() {
                1 => first,
                n => format!("{first} (+{} more)", n - 1),
            }
        }
        "ExitPlanMode" => plain(input["plan"].as_str()?),
        "Bash" => format!("$ {}", input["command"].as_str()?.split_whitespace().collect::<Vec<_>>().join(" ")),
        "WebFetch" => input["url"].as_str()?.to_string(),
        _ => return None,
    };
    Some(cap(&text, KEPT_CHARS)).filter(|s| !s.trim().is_empty())
}

fn answer_in_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    // Cut in the middle of a line.
    if start > 0 {
        lines.next();
    }
    answer_in(lines)
}

/// The text the agent wrote after its last tool call of the turn: none
/// when a tool came after it, as when the file lags behind the session.
fn answer_in<'a>(lines: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut answer: Option<(String, String)> = None;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["isSidechain"] == true || v["isMeta"] == true {
            continue;
        }
        let content = &v["message"]["content"];
        let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
        let has = |kind: &str| blocks.iter().any(|b| b["type"] == kind);
        match v["type"].as_str() {
            Some("user") => {
                if content.is_string() || has("text") || has("tool_result") {
                    answer = None;
                }
            }
            Some("assistant") => {
                let text: Vec<&str> = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
                let text = text.join("\n");
                let id = v["message"]["id"].as_str().unwrap_or_default().to_string();
                if has("tool_use") {
                    answer = None;
                } else if !text.trim().is_empty() {
                    // One answer may come in several lines, a block each.
                    answer = Some(match answer.take() {
                        Some((same, before)) if same == id && !id.is_empty() => (id, format!("{before}\n{text}")),
                        _ => (id, text),
                    });
                }
            }
            _ => {}
        }
    }
    answer.map(|(_, text)| text)
}

/// Markdown made plain text: no code blocks or tables, no marks; headings
/// end with a colon, list items start with a bullet. Bounded.
pub fn plain(markdown: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut fence: Option<&str> = None;
    for line in markdown.lines() {
        let line = line.trim();
        if let Some(mark) = fence {
            if line.starts_with(mark) {
                fence = None;
            }
            continue;
        }
        if let Some(mark) = ["```", "~~~"].into_iter().find(|m| line.starts_with(m)) {
            fence = Some(mark);
            continue;
        }
        if line.starts_with('|') || (line.len() >= 3 && line.chars().all(|c| matches!(c, '-' | '*' | '_' | ' '))) {
            continue;
        }
        let mut line = line.trim_start_matches('>').trim();
        let heading = line.starts_with('#');
        line = line.trim_start_matches('#').trim();
        let bullet = ["- ", "* ", "+ "].iter().find_map(|b| line.strip_prefix(b));
        let mut text = inline(bullet.unwrap_or(line));
        if text.is_empty() {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            continue;
        }
        if heading && !text.ends_with(|c: char| c.is_ascii_punctuation()) {
            text.push(':');
        }
        if bullet.is_some() {
            text = format!("• {text}");
        }
        out.push(text);
        if out.len() > KEPT_LINES {
            break;
        }
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    let more = out.len() > KEPT_LINES;
    out.truncate(KEPT_LINES);
    let text = out.join("\n");
    if more { format!("{}…", cap(&text, KEPT_CHARS - 1).trim_end_matches('…')) } else { cap(&text, KEPT_CHARS) }
}

/// A line without its marks: `code`, **bold**, *italic*, ~~struck~~,
/// [links](url) and images by their text.
fn inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '`' => {}
            '!' if chars.get(i + 1) == Some(&'[') => {}
            '[' => {
                // [text](url): the text, the url left out.
                if let Some(close) = chars[i..].iter().position(|&c| c == ']').map(|p| p + i) {
                    if chars.get(close + 1) == Some(&'(') {
                        if let Some(end) = chars[close..].iter().position(|&c| c == ')').map(|p| p + close) {
                            out.extend(&chars[i + 1..close]);
                            i = end + 1;
                            continue;
                        }
                    }
                }
                out.push(c);
            }
            '*' | '~' => {
                let before = i.checked_sub(1).map(|j| chars[j]);
                let after = chars.get(i + 1).copied();
                let spaced = |c: Option<char>| c.is_none_or(char::is_whitespace);
                // A mark hugs a word; "a * b" is arithmetic.
                if spaced(before) && spaced(after) || c == '~' && after != Some('~') && before != Some('~') {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
        i += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first of it, for a row of two or three lines: one paragraph, its
/// lines joined, a lone heading ("Summary:") left out.
pub fn lead(plain: &str, max: usize) -> String {
    let mut lines: Vec<&str> = plain.lines().map(str::trim).collect();
    let label = |l: &str| l.ends_with(':') && l.split_whitespace().count() <= 3;
    while lines.len() > 1 && (lines[0].is_empty() || label(lines[0])) {
        lines.remove(0);
    }
    let mut out = String::new();
    for line in lines {
        if out.chars().count() >= max {
            break;
        }
        let line = line.trim_start_matches("• ");
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(if out.ends_with(|c: char| ".!?:;,…".contains(c)) { " " } else { ". " });
        }
        out.push_str(line);
    }
    cap(&out, max)
}

/// Kept with a task: whole for the tooltip, its start for the row.
#[derive(Clone, Debug, PartialEq)]
pub struct Said {
    pub whole: gpui_kit::SharedString,
    pub lead: gpui_kit::SharedString,
}

/// The row shows three lines at most, of a narrow sidebar.
const LEAD_CHARS: usize = 180;

impl Said {
    pub fn new(text: &str) -> Option<Self> {
        let lead = lead(text, LEAD_CHARS);
        (!lead.is_empty()).then(|| Self { whole: cap(text, KEPT_CHARS).into(), lead: lead.into() })
    }
}

/// A desktop notification's text: what the agent asks, or what it did.
pub fn notice(status: &crate::agent::AgentStatus, said: Option<&str>) -> String {
    match (status, said) {
        (crate::agent::AgentStatus::NeedsInput(message), Some(said)) => format!("{message}\n{}", lead(said, 140)),
        (crate::agent::AgentStatus::NeedsInput(message), None) => message.clone(),
        (_, said) => said.map(|said| lead(said, 160)).filter(|s| !s.is_empty()).unwrap_or_else(|| "Finished".into()),
    }
}

/// At most `max` characters, cut at a word when there is one near.
pub fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    let cut = match cut.rfind(char::is_whitespace) {
        Some(at) if cut[..at].chars().count() > max * 3 / 4 => &cut[..at],
        _ => &cut,
    };
    format!("{}…", cut.trim_end_matches(|c: char| c.is_whitespace() || ",.;:".contains(c)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(v: Value) -> String {
        v.to_string()
    }

    fn assistant(id: &str, content: Value) -> String {
        line(json!({"type":"assistant","isSidechain":false,"message":{"id":id,"role":"assistant","content":content}}))
    }

    fn prompt(text: &str) -> String {
        line(json!({"type":"user","isSidechain":false,"message":{"role":"user","content":text}}))
    }

    fn result() -> String {
        line(json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}))
    }

    #[test]
    fn the_answer_is_what_came_after_the_last_tool() {
        let lines = [
            prompt("Fix the login"),
            assistant("m1", json!([{"type":"text","text":"Let me look."}])),
            assistant("m1", json!([{"type":"tool_use","id":"t1","name":"Read","input":{}}])),
            result(),
            assistant("m2", json!([{"type":"text","text":"Fixed the login."}])),
            assistant("m2", json!([{"type":"text","text":"Tests pass."}])),
            line(json!({"type":"system","subtype":"stop_hook_summary"})),
            line(json!({"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"a subagent"}]}})),
        ];
        assert_eq!(answer_in(lines.iter().map(String::as_str)).as_deref(), Some("Fixed the login.\nTests pass."));
    }

    #[test]
    fn a_transcript_behind_the_session_gives_nothing_old() {
        // The turn's last tool is written, its answer not yet.
        let lines = [prompt("Go"), assistant("m1", json!([{"type":"text","text":"Running tests."}])), assistant("m1", json!([{"type":"tool_use","id":"t","name":"Bash","input":{}}])), result()];
        assert_eq!(answer_in(lines.iter().map(String::as_str)), None);
        // The new prompt is, its answer not.
        let lines = [assistant("m0", json!([{"type":"text","text":"Earlier answer."}])), prompt("Next")];
        assert_eq!(answer_in(lines.iter().map(String::as_str)), None);
        let lines = ["{not json", "", "[]"];
        assert_eq!(answer_in(lines.into_iter()), None);
    }

    #[test]
    fn a_tail_starts_at_a_whole_line() {
        let dir = std::env::temp_dir().join(format!("ideae-said-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let filler = prompt(&"x".repeat(TAIL_BYTES as usize));
        let text = [filler, prompt("Go"), assistant("m", json!([{"type":"text","text":"## Done\nAll **green**."}]))].join("\n") + "\n";
        std::fs::write(&path, text).unwrap();
        let words = last_words(&json!({"transcript_path": path}));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(words.as_deref(), Some("Done:\nAll green."));
        assert_eq!(last_words(&json!({"last_assistant_message":"Given *directly*.","transcript_path":"/nowhere"})).as_deref(), Some("Given directly."));
        assert_eq!(last_words(&json!({"transcript_path":"/nowhere"})), None);
    }

    #[test]
    fn markdown_made_plain() {
        let md = "## Summary\n\nI fixed the **login** in `auth.rs`:\n\n- Checked the [token](https://example.com/t) expiry\n* Added a test\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n\n---\n> Note: 2 * 3 is ~~seven~~ six.";
        assert_eq!(
            plain(md),
            "Summary:\n\nI fixed the login in auth.rs:\n\n• Checked the token expiry\n• Added a test\n\nNote: 2 * 3 is seven six."
        );
        assert_eq!(lead(&plain(md), 200), "I fixed the login in auth.rs: Checked the token expiry. Added a test. Note: 2 * 3 is seven six.");
    }

    #[test]
    fn long_words_are_cut_short() {
        let long = "word ".repeat(400);
        let p = plain(&long);
        assert!(p.chars().count() <= KEPT_CHARS && p.ends_with('…'), "{p}");
        let many = (0..40).map(|i| format!("- item {i}")).collect::<Vec<_>>().join("\n");
        let p = plain(&many);
        assert_eq!(p.lines().count(), KEPT_LINES);
        assert!(p.ends_with('…'));
        assert_eq!(cap("Fixed the login and the logout", 20), "Fixed the login and…");
        assert_eq!(cap("short", 20), "short");
        assert_eq!(lead("Done:", 50), "Done:");
        assert_eq!(cap("ünïcödé", 4), "ünï…");
    }

    #[test]
    fn what_a_tool_waits_for() {
        let q = json!({"tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which database?","options":[]},{"question":"Port?"}]}});
        assert_eq!(asks(&q).as_deref(), Some("Which database? (+1 more)"));
        assert_eq!(asks(&json!({"tool_name":"Bash","tool_input":{"command":"cargo  test\n --all"}})).as_deref(), Some("$ cargo test --all"));
        assert_eq!(asks(&json!({"tool_name":"ExitPlanMode","tool_input":{"plan":"# Plan\n1. Do it"}})).as_deref(), Some("Plan:\n1. Do it"));
        assert_eq!(asks(&json!({"tool_name":"Read","tool_input":{"file_path":"/x"}})), None);
        assert_eq!(asks(&json!({"tool_name":"AskUserQuestion","tool_input":{}})), None);
    }
}
