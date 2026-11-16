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

/// The agent's latest answer, in its own words: what it says it did.
pub fn last_answer(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut last = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        // Not what a subagent told the agent.
        if v["type"] != "assistant" || v["isSidechain"] == true {
            continue;
        }
        let text: Vec<&str> =
            v["message"]["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
        let text = text.join("\n").trim().to_string();
        if !text.is_empty() {
            last = Some(text);
        }
    }
    last
}

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
    /// What the answers of the last day took: input not read from the
    /// cache, and output.
    pub recent: u64,
}

impl Usage {
    /// The session so far, what was read from the cache again not counted.
    pub fn spent(&self) -> u64 {
        self.input + self.output
    }

    /// How full the context is, 0 to 1: of 200k tokens, or of a million
    /// once it holds more than the smaller window can.
    pub fn fill(&self) -> f32 {
        let window = if self.context > 200_000 || self.model.contains("[1m]") { 1_000_000 } else { 200_000 };
        (self.context as f32 / window as f32).min(1.)
    }
}

/// Seconds since 1970 of a time as transcripts write it: `2026-11-15T19:45:55.123Z`.
fn epoch(time: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| time.get(range)?.parse::<u64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // Days from the civil date, the year starting in March.
    let year = if month <= 2 { year - 1 } else { year };
    let (era, of_era) = (year / 400, year % 400);
    let of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    Some((days.checked_sub(719_468)?) * 86_400 + hour * 3600 + minute * 60 + second)
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

/// What was read of a transcript so far: a session only grows, so the next
/// read takes up where this one ended.
#[derive(Default)]
struct Tail {
    /// Up to where whole lines were read.
    offset: u64,
    /// A message is written again as it grows: the last copy of each counts.
    answers: HashMap<String, (u64, u64, u64, u64)>,
    context: u64,
    model: String,
    used: u64,
}

/// The transcripts read lately, by path. A few bytes per answer; the ones
/// not asked about for longest make room.
static TAILS: std::sync::Mutex<Option<HashMap<PathBuf, Tail>>> = std::sync::Mutex::new(None);
static READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const TAILS_KEPT: usize = 48;

impl Tail {
    fn take(&mut self, line: &[u8]) {
        if memchr(line, b"\"usage\"").is_none() {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
        let message = &v["message"];
        let u = &message["usage"];
        if v["type"] != "assistant" || !u.is_object() {
            return;
        }
        let n = |key: &str| u[key].as_u64().unwrap_or(0);
        let (input, created, read, output) = (n("input_tokens"), n("cache_creation_input_tokens"), n("cache_read_input_tokens"), n("output_tokens"));
        let id = message["id"].as_str().unwrap_or_default().to_string();
        let at = v["timestamp"].as_str().and_then(epoch).unwrap_or(0);
        self.answers.insert(id, (input + created, read, output, at));
        self.context = input + created + read;
        if let Some(model) = message["model"].as_str().filter(|m| !m.starts_with('<')) {
            self.model = model.to_string();
        }
    }

    fn usage(&self) -> Usage {
        let mut usage = Usage { model: self.model.clone(), context: self.context, answers: self.answers.len() as u32, ..Usage::default() };
        let day_ago = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |now| now.as_secs().saturating_sub(86_400));
        for (input, read, output, at) in self.answers.values() {
            usage.input += input;
            usage.cache_read += read;
            usage.output += output;
            if *at >= day_ago {
                usage.recent += input + output;
            }
        }
        usage
    }
}

fn memchr(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|window| window == needle)
}

/// What a session has used. Slow for a long one the first time only: not
/// for the UI thread.
pub fn read(path: &Path) -> Option<Usage> {
    use std::io::Seek as _;
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut tail = TAILS.lock().unwrap().get_or_insert_with(HashMap::new).remove(path).unwrap_or_default();
    // Shorter than what was read: another file under the name.
    if length < tail.offset {
        tail = Tail::default();
    }
    file.seek(std::io::SeekFrom::Start(tail.offset)).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.take(&line);
                // A line still being written is read again when it is whole.
                if line.last() != Some(&b'\n') {
                    break;
                }
                tail.offset += n as u64;
            }
        }
    }
    let usage = tail.usage();
    tail.used = READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tails = TAILS.lock().unwrap();
    let tails = tails.get_or_insert_with(HashMap::new);
    tails.insert(path.to_path_buf(), tail);
    while tails.len() > TAILS_KEPT {
        let Some(oldest) = tails.iter().min_by_key(|(_, tail)| tail.used).map(|(path, _)| path.clone()) else { break };
        tails.remove(&oldest);
    }
    Some(usage)
}

pub fn tokens(n: u64) -> String {
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

    use super::{last_answer, read, sessions_dir};

    #[test]
    fn the_last_answer_is_the_agents_own_last_words() {
        let path = std::env::temp_dir().join(format!("ideae-answer-{}.jsonl", std::process::id()));
        let lines = [
            r#"{"type":"user","message":{"content":"Fix it"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Read"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Fixed the login.\nTests pass."}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"A subagent's report."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        assert_eq!(last_answer(&path).as_deref(), Some("Fixed the login.\nTests pass."));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_last_day_and_how_full_the_context_is() {
        use super::{Usage, epoch};
        assert_eq!(epoch("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(epoch("2026-11-15T19:45:55.123Z"), Some(1_794_771_955));
        assert_eq!(epoch("2024-02-29T12:00:00Z"), Some(1_709_208_000), "a leap day");
        assert_eq!(epoch("yesterday"), None);

        let path = std::env::temp_dir().join(format!("ideae-recent-{}.jsonl", std::process::id()));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        // Back to a date: the inverse of what is read.
        let written = |ago: u64| {
            let at = now - ago;
            let (days, of_day) = (at / 86_400, at % 86_400);
            let shifted = days + 719_468;
            let (era, of_era) = (shifted / 146_097, shifted % 146_097);
            let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
            let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
            let month_part = (5 * of_year + 2) / 153;
            let (day, month) = (of_year - (153 * month_part + 2) / 5 + 1, if month_part < 10 { month_part + 3 } else { month_part - 9 });
            let year = year_of_era + era * 400 + u64::from(month <= 2);
            format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z", of_day / 3600, of_day % 3600 / 60, of_day % 60)
        };
        let answer = |id: &str, ago: u64, input: u64, read: u64, output: u64| {
            format!(
                r#"{{"type":"assistant","timestamp":"{}","message":{{"id":"{id}","model":"claude-x","usage":{{"input_tokens":{input},"cache_creation_input_tokens":0,"cache_read_input_tokens":{read},"output_tokens":{output}}}}}}}"#,
                written(ago)
            )
        };
        std::fs::write(&path, [answer("old", 3 * 86_400, 1000, 0, 500), answer("new", 3600, 200, 149_800, 50)].join("\n")).unwrap();
        let usage = read(&path).unwrap();
        assert_eq!((usage.spent(), usage.recent), (1750, 250), "of the last day only the later answer");
        assert_eq!(usage.fill(), 0.75, "150k of 200k");
        assert_eq!(Usage { context: 300_000, ..Usage::default() }.fill(), 0.3, "more than the small window holds: it is the large one");
        let _ = std::fs::remove_file(&path);
    }

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
        // More is written: only that is read, a line begun is not counted twice, and an answer written again replaces itself.
        let mut more = lines.join("\n");
        more.push('\n');
        more.push_str(&answer("b", 20, 300, 9));
        more.push('\n');
        more.push_str(&answer("c", 5, 400, 2));
        std::fs::write(&path, &more).unwrap();
        let u = read(&path).unwrap();
        assert_eq!((u.answers, u.input, u.cache_read, u.output, u.context), (3, 35, 800, 16, 405));
        let read_to = super::TAILS.lock().unwrap().as_ref().unwrap()[&path].offset;
        assert_eq!(read_to as usize, more.rfind('\n').unwrap() + 1, "up to the last whole line");
        assert_eq!(read(&path).unwrap(), u, "nothing new: the same");
        // Another file under the name is read from its start.
        std::fs::write(&path, answer("z", 1, 2, 3)).unwrap();
        assert_eq!(read(&path).unwrap().answers, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sessions_dir_dashes_the_path() {
        let dir = sessions_dir(Path::new("/ott/a.worktrees/b_c")).unwrap();
        assert!(dir.ends_with("projects/-ott-a-worktrees-b-c"), "{dir:?}");
    }
}
