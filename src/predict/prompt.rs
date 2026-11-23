//! What the model is asked and what its answer changes: the recent edits,
//! the prompt of each format, the answer turned into gray text at the
//! cursor and the edits around it.

use std::{collections::VecDeque, ops::Range, sync::Arc, time::{Duration, Instant}};

use gpui_kit::component::input::{Rope, RopeExt as _};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Sweep,
    Zeta,
}

impl Format {
    pub fn stops(self) -> &'static [&'static str] {
        match self {
            Format::Sweep => &["<|file_sep|>", "</s>", "<|endoftext|>"],
            Format::Zeta => &[ZETA_EOS],
        }
    }
}

const ZETA_EOS: &str = "<[end▁of▁sentence]>";
const CURSOR: &str = "<|user_cursor|>";
/// Edits closer together than this are one change in the history.
pub const GROUP: Duration = Duration::from_secs(1);
const MAX_HUNKS: usize = 6;
const MAX_FILES: usize = 8;
/// A change bigger than this (a paste, a reload) is left out of the history.
const MAX_HUNK_LINES: usize = 80;

/// One change made earlier: its lines, marked ' ', '-' or '+', with three
/// lines of context, and where it starts in the file before and after.
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    pub path: Arc<str>,
    pub old_line: usize,
    pub new_line: usize,
    pub lines: Vec<(char, String)>,
}

impl Hunk {
    fn side(&self, keep: char, context: usize) -> String {
        let first = self.lines.iter().position(|(tag, _)| *tag != ' ').unwrap_or(0);
        let last = self.lines.iter().rposition(|(tag, _)| *tag != ' ').unwrap_or(0);
        let from = first.saturating_sub(context);
        let to = (last + 1 + context).min(self.lines.len());
        let lines: Vec<&str> = self.lines[from..to].iter().filter(|(tag, _)| *tag == ' ' || *tag == keep).map(|(_, l)| l.as_str()).collect();
        lines.join("\n")
    }

    /// As `diff -u` writes it, the way Zeta was taught.
    fn unified(&self, root: &str) -> String {
        let count = |tag| self.lines.iter().filter(|(t, _)| *t == ' ' || *t == tag).count();
        let range = |start: usize, len: usize| match len {
            1 => format!("{}", start + 1),
            0 => format!("{start},0"),
            _ => format!("{},{len}", start + 1),
        };
        let mut out = format!("--- a/{root}/{}\n+++ b/{root}/{}\n", self.path, self.path);
        out += &format!("@@ -{} +{} @@\n", range(self.old_line, count('-')), range(self.new_line, count('+')));
        for (tag, line) in &self.lines {
            out.push(*tag);
            out += line;
            out.push('\n');
        }
        out
    }
}

/// The lines of `a` and `b` that differ, as hunks; none for a change too big to tell.
pub fn diff(a: &Rope, b: &Rope, path: &Arc<str>) -> Vec<Hunk> {
    // The empty line after the last newline is no line of the file.
    let rows = |r: &Rope| r.lines_len() - usize::from(r.len() == 0 || r.char_at(r.len() - 1) == Some('\n'));
    let (na, nb) = (rows(a), rows(b));
    let mut prefix = 0;
    while prefix < na.min(nb) && a.slice_line(prefix) == b.slice_line(prefix) {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (na - prefix).min(nb - prefix) && a.slice_line(na - 1 - suffix) == b.slice_line(nb - 1 - suffix) {
        suffix += 1;
    }
    if prefix + suffix == na && prefix + suffix == nb {
        return Vec::new();
    }
    if (na - prefix - suffix).max(nb - prefix - suffix) > MAX_HUNK_LINES {
        return Vec::new();
    }
    let from = prefix.saturating_sub(3);
    let (end_a, end_b) = ((na - suffix + 3).min(na), (nb - suffix + 3).min(nb));
    let lines = |rope: &Rope, range: Range<usize>| range.map(|row| rope.slice_line(row).to_string()).collect::<Vec<_>>();
    let (old, new) = (lines(a, from..end_a), lines(b, from..end_b));
    let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &old, &new);
    similar::group_diff_ops(ops, 3)
        .into_iter()
        .map(|group| {
            let mut hunk = Hunk { path: path.clone(), old_line: 0, new_line: 0, lines: Vec::new() };
            for (at, op) in group.iter().enumerate() {
                let (tag, old_range, new_range) = op.as_tag_tuple();
                if at == 0 {
                    hunk.old_line = from + old_range.start;
                    hunk.new_line = from + new_range.start;
                }
                match tag {
                    similar::DiffTag::Equal => hunk.lines.extend(old[old_range].iter().map(|l| (' ', l.clone()))),
                    similar::DiffTag::Delete => hunk.lines.extend(old[old_range].iter().map(|l| ('-', l.clone()))),
                    similar::DiffTag::Insert => hunk.lines.extend(new[new_range].iter().map(|l| ('+', l.clone()))),
                    similar::DiffTag::Replace => {
                        hunk.lines.extend(old[old_range].iter().map(|l| ('-', l.clone())));
                        hunk.lines.extend(new[new_range].iter().map(|l| ('+', l.clone())));
                    }
                }
            }
            hunk
        })
        .collect()
}

/// A stable part of the file put first in the prompt, so the server's
/// cache keeps it from one keystroke to the next.
#[derive(Clone, Debug)]
pub struct Context {
    pub lines: Range<usize>,
    pub text: String,
    /// The file's declarations, for Zeta.
    pub outline: String,
    groups: u32,
    to_the_end: bool,
}

impl Context {
    fn of(text: &Rope, line: usize) -> Self {
        let n = text.lines_len();
        let (mut first, mut end) = (line, (line + 1).min(n));
        let cursor_at = text.line_start_offset(line);
        while first > 0 && cursor_at - text.line_start_offset(first - 1) <= 2600 {
            first -= 1;
        }
        while end < n && text.line_start_offset(end + 1) - text.line_start_offset(line + 1) <= 2600 {
            end += 1;
        }
        let window = text.slice(text.line_start_offset(first)..text.line_start_offset(end)).to_string();
        Self { lines: first..end, text: window, outline: outline(text), groups: 0, to_the_end: end == n }
    }

    /// Whether the cursor's line is still well inside, and the changes
    /// since it was taken are few.
    fn holds(&self, line: usize) -> bool {
        (self.lines.start == 0 || line >= self.lines.start + 10) && (self.to_the_end || line + 10 <= self.lines.end) && self.groups < 30
    }
}

/// The lines that declare things, with "..." for what is between, as
/// related context for Zeta.
fn outline(text: &Rope) -> String {
    const WORDS: [&str; 18] = [
        "fn ", "struct ", "enum ", "impl ", "impl<", "trait ", "mod ", "const ", "static ", "type ", "class ", "def ", "func ", "function ",
        "interface ", "export ", "public ", "package ",
    ];
    let mut out = String::new();
    let mut previous = None;
    for row in 0..text.lines_len() {
        let line = text.slice_line(row);
        if line.len() > 300 {
            continue;
        }
        let line = line.to_string();
        let bare = line.trim_start();
        let bare = bare.strip_prefix("pub(crate) ").or_else(|| bare.strip_prefix("pub ")).unwrap_or(bare);
        let bare = bare.strip_prefix("async ").unwrap_or(bare);
        if !WORDS.iter().any(|w| bare.starts_with(w)) {
            continue;
        }
        if previous.is_none_or(|p| p + 1 != row) {
            out += "...\n";
        }
        out += &line;
        out.push('\n');
        previous = Some(row);
        if out.len() > 6000 {
            break;
        }
    }
    out += "...\n";
    out
}

struct Track {
    path: Arc<str>,
    start: Rope,
    last: Rope,
    at: Instant,
    pending: bool,
    context: Option<Context>,
}

/// The recent edits of the files typed in: per file the text as it was
/// before the change being made, and the changes before it.
#[derive(Default)]
pub struct Tracker {
    files: VecDeque<Track>,
    hunks: VecDeque<Hunk>,
}

/// What the tracker knows when a prediction is asked.
pub struct Seen {
    /// The file before the change being made.
    pub before: Rope,
    /// Earlier changes, the oldest first, of any file.
    pub hunks: Vec<Hunk>,
    pub context: Context,
}

impl Tracker {
    /// `first` is the file as it was opened, for a file not seen before.
    pub fn see(&mut self, path: &Arc<str>, text: &Rope, line: usize, first: Option<Rope>, now: Instant) -> Seen {
        for ix in 0..self.files.len() {
            let file = &self.files[ix];
            if file.pending && (*file.path != **path || now.duration_since(file.at) > GROUP) {
                self.close(ix);
            }
        }
        let ix = match self.files.iter().position(|f| f.path == *path) {
            Some(ix) => ix,
            None => {
                let start = first.unwrap_or_else(|| text.clone());
                if self.files.len() >= MAX_FILES {
                    self.files.pop_front();
                }
                self.files.push_back(Track { path: path.clone(), last: start.clone(), start, at: now, pending: false, context: None });
                self.files.len() - 1
            }
        };
        let file = &mut self.files[ix];
        file.last = text.clone();
        file.at = now;
        file.pending = true;
        if file.context.as_ref().is_none_or(|c| !c.holds(line)) {
            file.context = Some(Context::of(text, line));
        }
        Seen {
            before: file.start.clone(),
            hunks: self.hunks.iter().cloned().collect(),
            context: file.context.clone().unwrap_or_else(|| Context::of(text, line)),
        }
    }

    fn close(&mut self, ix: usize) {
        let file = &mut self.files[ix];
        file.pending = false;
        let hunks = diff(&file.start, &file.last, &file.path);
        file.start = file.last.clone();
        if hunks.is_empty() {
            return;
        }
        if let Some(context) = &mut file.context {
            context.groups += 1;
        }
        self.hunks.extend(hunks);
        while self.hunks.len() > MAX_HUNKS {
            self.hunks.pop_front();
        }
    }
}

/// A prompt, and what of the file the answer rewrites.
pub struct Prompt {
    pub text: String,
    pub format: Format,
    /// Bytes of the file the model writes anew.
    pub region: Range<usize>,
    /// Zeta: where each marker is, from the start of the region.
    marks: Vec<usize>,
}

pub struct Ask<'a> {
    pub format: Format,
    /// The file's path in the project, with forward slashes.
    pub path: &'a str,
    /// The project's folder name, Zeta's paths start with it.
    pub root: &'a str,
    pub text: &'a Rope,
    pub cursor: usize,
    pub seen: &'a Seen,
    /// The stable part of the file goes first; a model on the CPU is
    /// asked without it, for speed.
    pub with_context: bool,
}

pub fn build(ask: &Ask) -> Prompt {
    match ask.format {
        Format::Sweep => sweep(ask),
        Format::Zeta => zeta(ask),
    }
}

fn rows(text: &Rope, rows: Range<usize>) -> Range<usize> {
    text.line_start_offset(rows.start)..text.line_start_offset(rows.end)
}

fn sweep(ask: &Ask) -> Prompt {
    let Ask { path, text, cursor, seen, .. } = *ask;
    let line = text.offset_to_point(cursor).row;
    let n = text.lines_len();
    let (first, end) = (line.saturating_sub(10), (line + 11).min(n));
    let region = rows(text, first..end);
    let delta = n as isize - seen.before.lines_len() as isize;
    let before_end = (end as isize - delta).clamp(first as isize, seen.before.lines_len() as isize) as usize;
    let original = seen.before.slice(rows(&seen.before, first..before_end)).to_string();
    let current = text.slice(region.clone()).to_string();
    let mut parts: Vec<String> = Vec::new();
    if ask.with_context {
        parts.push(format!("<|file_sep|>{path}"));
        parts.push(seen.context.text.trim_end_matches('\n').to_string());
    }
    for hunk in &seen.hunks {
        parts.push(format!("<|file_sep|>{}.diff", hunk.path));
        parts.push("original:".into());
        parts.push(hunk.side('-', 1));
        parts.push("updated:".into());
        parts.push(hunk.side('+', 1));
    }
    parts.push(format!("<|file_sep|>original/{path}"));
    parts.push(original.trim_end_matches('\n').to_string());
    parts.push(format!("<|file_sep|>current/{path}"));
    parts.push(current.trim_end_matches('\n').to_string());
    parts.push(format!("<|file_sep|>updated/{path}"));
    Prompt { text: parts.join("\n") + "\n", format: Format::Sweep, region, marks: Vec::new() }
}

/// Lines around the cursor, about `before` and `after` bytes each way.
fn around(text: &Rope, cursor: usize, before: usize, after: usize) -> Range<usize> {
    let n = text.lines_len();
    let line = text.offset_to_point(cursor).row;
    let (mut first, mut end) = (line, (line + 1).min(n));
    while first > 0 && text.line_start_offset(line) - text.line_start_offset(first - 1) <= before {
        first -= 1;
    }
    while end < n && text.line_start_offset(end + 1) - text.line_start_offset(line + 1) <= after {
        end += 1;
    }
    rows(text, first..end)
}

fn good_start(line: &str) -> bool {
    let t = line.trim();
    !(t.is_empty() || t.starts_with(['}', ']', ')']) || ["break", "continue", "return", "throw", "end"].contains(&t.trim_end_matches(';')))
}

/// Where Zeta's markers go: blocks of 6 to 16 lines, cut at blank lines,
/// not before a closing bracket.
fn markers(text: &str) -> Vec<usize> {
    const LO: usize = 6;
    const HI: usize = 16;
    if text.is_empty() {
        return vec![0, 0];
    }
    let mut starts = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        starts.push((at, line));
        at += line.len();
    }
    let blank: Vec<bool> = starts.iter().map(|(_, l)| l.trim().is_empty()).collect();
    let good: Vec<bool> = starts.iter().map(|(_, l)| good_start(l)).collect();
    let nudge = |from: usize| (from..(from + 5).min(starts.len())).find(|&j| good[j]).unwrap_or(from);
    let (mut offs, mut last, mut i) = (vec![0], 0, 0);
    while i < starts.len() {
        let gap = i - last;
        if gap >= LO && !blank[i] && i > 0 && blank[i - 1] {
            let t = if good[i] { i } else { nudge(i) };
            if starts.len() - t >= LO && starts[t].0 > *offs.last().unwrap() {
                offs.push(starts[t].0);
                last = t;
                i = t + 1;
                continue;
            }
        }
        if gap >= HI {
            let t = nudge(i);
            if starts[t].0 > *offs.last().unwrap() {
                offs.push(starts[t].0);
                last = t;
                i = t + 1;
                continue;
            }
        }
        i += 1;
    }
    if *offs.last().unwrap() != text.len() {
        offs.push(text.len());
    }
    offs
}

fn zeta(ask: &Ask) -> Prompt {
    let Ask { path, root, text, cursor, seen, .. } = *ask;
    let editable = around(text, cursor, 520, 520);
    let outer = around(text, cursor, 970, 970);
    let body = text.slice(editable.clone()).to_string();
    let marks = markers(&body);
    let mut section = format!("<filename>{path}\n") + &text.slice(outer.start..editable.start).to_string();
    let at = cursor - editable.start;
    let mut placed = false;
    for (i, &o) in marks.iter().enumerate() {
        section += &format!("<|marker_{}|>", i + 1);
        if let Some(&next) = marks.get(i + 1) {
            let block = &body[o..next];
            if !placed && o <= at && at <= next {
                placed = true;
                section += &block[..at - o];
                section += CURSOR;
                section += &block[at - o..];
            } else {
                section += block;
            }
        }
    }
    if !section.ends_with('\n') {
        section.push('\n');
    }
    let suffix = text.slice(editable.end..outer.end).to_string();
    let mut prompt = format!("<[fim-suffix]>{suffix}{}<[fim-prefix]>", if suffix.ends_with('\n') { "" } else { "\n" });
    if ask.with_context {
        prompt += &format!("<filename>{root}/{path}\n{}\n", seen.context.outline);
    }
    let shown: Arc<str> = path.into();
    let history: String = seen.hunks.iter().chain(diff(&seen.before, text, &shown).iter()).map(|h| h.unified(root)).collect();
    if !history.is_empty() {
        prompt += &format!("<filename>edit_history\n{history}\n");
    }
    prompt += &section;
    prompt += "<[fim-middle]>";
    Prompt { text: prompt, format: Format::Zeta, region: editable, marks }
}

/// The model's answer: the bytes of the file it rewrote, their new text,
/// and where in it the model put the cursor (Zeta marks it; Sweep does not).
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub range: Range<usize>,
    pub new: String,
    pub caret: Option<usize>,
}

/// The model's answer as a replacement: what of the file, by what.
pub fn parse(prompt: &Prompt, text: &Rope, output: &str) -> Option<Answer> {
    match prompt.format {
        Format::Sweep => {
            let body = Format::Sweep.stops().iter().fold(output, |out, stop| out.split(stop).next().unwrap_or(out));
            let old = text.slice(prompt.region.clone()).to_string();
            let tail = &old[old.trim_end_matches(['\r', '\n']).len()..];
            Some(Answer { range: prompt.region.clone(), new: body.trim_end_matches(['\r', '\n']).to_string() + tail, caret: None })
        }
        Format::Zeta => {
            let out = output.split(ZETA_EOS).next().unwrap_or_default();
            let tags = marker_tags(out);
            let (first, last) = (tags.first()?, tags.last()?);
            if tags.len() < 2 || first.2 == last.2 {
                return None;
            }
            let (a, b) = (first.2.checked_sub(1)?, last.2.checked_sub(1)?);
            if a >= b || b >= prompt.marks.len() {
                return None;
            }
            let mut body = String::new();
            let mut at = first.1;
            for tag in &tags[1..] {
                body += &out[at..tag.0];
                at = tag.1;
            }
            let caret = body.find(CURSOR);
            let body = body.replace(CURSOR, "");
            let start = prompt.region.start;
            Some(Answer { range: start + prompt.marks[a]..start + prompt.marks[b], new: body, caret })
        }
    }
}

/// `<|marker_N|>` in `text`: where each starts and ends, and N.
fn marker_tags(text: &str) -> Vec<(usize, usize, usize)> {
    let mut tags = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find("<|marker_") {
        let start = from + found;
        let digits = start + "<|marker_".len();
        let len = text[digits..].bytes().take_while(u8::is_ascii_digit).count();
        if len > 0 && text[digits + len..].starts_with("|>") {
            let end = digits + len + 2;
            tags.push((start, end, text[digits..digits + len].parse().unwrap_or(0)));
            from = end;
        } else {
            from = digits;
        }
    }
    tags
}

/// What the editor shows at the cursor: text to insert there, and the end
/// of the text it replaces, on the cursor's line, when not only inserted.
#[derive(Debug, PartialEq)]
pub struct Suggestion {
    pub insert: String,
    pub replaces_to: Option<usize>,
}

/// An answer as the editor shows it: what is typed at the cursor, and
/// every change, that one among them.
#[derive(Debug, PartialEq)]
pub struct Prediction {
    pub ghost: Option<Suggestion>,
    /// Bytes of the text and what replaces them, in order and apart.
    pub edits: Vec<(Range<usize>, String)>,
    /// Where the model put the caret, in the text once edited.
    pub caret: Option<usize>,
}

/// The change of the answer that is at the cursor, as a suggestion there;
/// `None` when it cannot be shown as text typed there.
#[cfg(test)]
pub fn suggestion(text: &Rope, cursor: usize, answer: Answer) -> Option<Suggestion> {
    prediction(text, cursor, answer).and_then(|p| p.ghost)
}

/// The answer as changes: the one at the cursor as text typed there when it
/// can be, the others as the few words each replaces.
pub fn prediction(text: &Rope, cursor: usize, answer: Answer) -> Option<Prediction> {
    let Answer { range, new, caret } = answer;
    let old = text.slice(range.clone()).to_string();
    let changes = line_changes(&old, &new);
    let cursor_line = text.offset_to_point(cursor).row - text.offset_to_point(range.start).row;
    let at = changes.iter().position(|c| {
        c.lines.contains(&cursor_line) || (c.lines.is_empty() && (c.lines.start == cursor_line + 1 || c.lines.start == cursor_line))
    });
    let mut ghost = None;
    let mut edits = Vec::new();
    for (i, change) in changes.iter().enumerate() {
        let base = range.start + change.old.start;
        let mut hunks: Vec<(Range<usize>, String)> =
            small(&old[change.old.clone()], &new[change.new.clone()]).into_iter().map(|(r, s)| (base + r.start..base + r.end, s)).collect();
        if Some(i) == at
            && let Some((rest, edit, shown)) = take_ghost(text, cursor, &hunks)
        {
            edits.push(edit);
            ghost = Some(shown);
            hunks = rest;
        }
        for (r, s) in hunks {
            let (r, s) = slide(text, r, s, range.start);
            if s.trim().is_empty() && text.slice(r.clone()).to_string().trim().is_empty() {
                continue;
            }
            edits.push((r, s));
        }
    }
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(r, _)| r.start);
    Some(Prediction { ghost, edits, caret: caret.map(|c| range.start + c) })
}

/// Of `hunks`, those on the cursor's line (or lines inserted after it) as
/// text typed at the cursor: the hunks left, the edit, and how it shows.
fn take_ghost(text: &Rope, cursor: usize, hunks: &[(Range<usize>, String)]) -> Option<(Vec<(Range<usize>, String)>, (Range<usize>, String), Suggestion)> {
    let row = text.offset_to_point(cursor).row;
    let (bol, eol) = (text.line_start_offset(row), text.line_end_offset(row));
    let mine: Vec<usize> = (0..hunks.len())
        .filter(|&i| {
            let r = &hunks[i].0;
            (r.start >= bol && r.end <= eol) || (r.is_empty() && r.start == eol + 1)
        })
        .collect();
    let (&first, &last) = (mine.first()?, mine.last()?);
    let (start, end) = (hunks[first].0.start, hunks[last].0.end);
    let mut insert = String::new();
    let mut at = start;
    for (r, s) in &hunks[first..=last] {
        insert += &text.slice(at..r.start).to_string();
        insert += s;
        at = r.end;
    }
    let (edit, shown) = ghost(text, cursor, start, end, insert)?;
    let rest = hunks[..first].iter().chain(&hunks[last + 1..]).cloned().collect();
    Some((rest, edit, shown))
}

/// Lines of `old` that `new` changes, those next to each other as one: as
/// bytes of `old`, bytes of `new`, and lines of `old`.
struct LineChange {
    old: Range<usize>,
    new: Range<usize>,
    lines: Range<usize>,
}

fn line_changes(old: &str, new: &str) -> Vec<LineChange> {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let starts = |lines: &[&str]| {
        let mut at = vec![0];
        for line in lines {
            at.push(at.last().unwrap() + line.len());
        }
        at
    };
    let (sa, sb) = (starts(&a), starts(&b));
    let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &a, &b);
    joined(ops).into_iter().map(|(ra, rb)| LineChange { old: sa[ra.start]..sa[ra.end], new: sb[rb.start]..sb[rb.end], lines: ra }).collect()
}

/// The ranges of a diff that differ, those next to each other as one.
fn joined(ops: Vec<similar::DiffOp>) -> Vec<(Range<usize>, Range<usize>)> {
    let mut changes: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    let mut next_to = false;
    for op in ops {
        let (tag, ra, rb) = op.as_tag_tuple();
        if tag == similar::DiffTag::Equal {
            next_to = false;
            continue;
        }
        match changes.last_mut() {
            Some((la, lb)) if next_to => {
                la.end = ra.end;
                lb.end = rb.end;
            }
            _ => changes.push((ra, rb)),
        }
        next_to = true;
    }
    changes
}

/// How many bytes `old` and `new` have the same at their start and at their end.
fn common(old: &str, new: &str) -> (usize, usize) {
    let mut prefix = old.bytes().zip(new.bytes()).take_while(|(x, y)| x == y).count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let mut suffix = old[prefix..].bytes().rev().zip(new[prefix..].bytes().rev()).take_while(|(x, y)| x == y).count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    (prefix, suffix)
}

/// `start..end` replaced by `insert` as text typed at the cursor, and the
/// edit that is.
fn ghost(text: &Rope, cursor: usize, mut start: usize, mut end: usize, mut insert: String) -> Option<((Range<usize>, String), Suggestion)> {
    if start == end {
        while start > cursor && text.chars_at(start).reversed().next().is_some_and(|c| insert.ends_with(c)) {
            let c = insert.pop().unwrap();
            insert.insert(0, c);
            start -= c.len_utf8();
        }
        while start < cursor && text.char_at(start).is_some_and(|c| insert.starts_with(c)) {
            let c = insert.remove(0);
            insert.push(c);
            start += c.len_utf8();
        }
        end = start;
    }
    let removed = text.slice(start..end).to_string();
    if insert.trim().is_empty() && removed.trim().is_empty() {
        return None;
    }
    let eol = text.line_end_offset(text.offset_to_point(cursor).row);
    // Further on, it is shown where it is: what it replaces can be seen.
    if start != cursor || end > eol {
        return None;
    }
    if end == cursor && cursor == eol {
        return Some(((cursor..cursor, insert.clone()), Suggestion { insert, replaces_to: None }));
    }
    let line = insert + &text.slice(end..eol).to_string();
    (line != text.slice(cursor..eol).to_string()).then(|| ((cursor..eol, line.clone()), Suggestion { insert: line, replaces_to: Some(eol) }))
}

/// Where the words, the runs of blanks and every other character of `s`
/// start, and its end.
fn tokens(s: &str) -> Vec<usize> {
    let kind = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c == ' ' || c == '\t' {
            1
        } else {
            2
        }
    };
    let mut starts = Vec::new();
    let mut previous = None;
    for (i, c) in s.char_indices() {
        let k = kind(c);
        if k == 2 || previous != Some(k) {
            starts.push(i);
        }
        previous = Some(k);
    }
    starts.push(s.len());
    starts
}

/// The changes of `new` to `old` word by word, as bytes of `old` and their
/// new text; all that differs as one when they are too many to read.
fn small(old: &str, new: &str) -> Vec<(Range<usize>, String)> {
    const MOST: usize = 6;
    let (sa, sb) = (tokens(old), tokens(new));
    let words = |s: &str, at: &[usize]| at.windows(2).map(|w| s[w[0]..w[1]].to_string()).collect::<Vec<_>>();
    let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &words(old, &sa), &words(new, &sb));
    let changes = joined(ops);
    if changes.len() > MOST {
        let (prefix, suffix) = common(old, new);
        return vec![(prefix..old.len() - suffix, new[prefix..new.len() - suffix].to_string())];
    }
    changes
        .into_iter()
        .map(|(ra, rb)| {
            let (ra, rb) = (sa[ra.start]..sa[ra.end], sb[rb.start]..sb[rb.end]);
            let (o, n) = (&old[ra.clone()], &new[rb.clone()]);
            let (mut prefix, mut suffix) = common(o, n);
            // A word changed is shown whole, one only added to or cut short is not.
            if prefix + suffix < o.len().min(n.len()) {
                (prefix, suffix) = (0, 0);
            }
            (ra.start + prefix..ra.end - suffix, n[prefix..n.len() - suffix].to_string())
        })
        .filter(|(r, s)| !r.is_empty() || !s.is_empty())
        .collect()
}

/// Whole lines inserted become an insertion at the end of the line before
/// them, where the editor shows them as typed after it.
fn slide(text: &Rope, mut range: Range<usize>, mut new: String, floor: usize) -> (Range<usize>, String) {
    if !range.is_empty() || !new.contains('\n') {
        return (range, new);
    }
    while range.start > floor && !new.starts_with('\n') && text.chars_at(range.start).reversed().next().is_some_and(|c| new.ends_with(c)) {
        let c = new.pop().unwrap();
        new.insert(0, c);
        range.start -= c.len_utf8();
    }
    range.end = range.start;
    (range, new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(before: &str, hunks: Vec<Hunk>) -> Seen {
        let before = Rope::from(before);
        Seen { context: Context::of(&before, 0), before, hunks }
    }

    fn answer(range: Range<usize>, new: String) -> Answer {
        Answer { range, new, caret: None }
    }

    #[test]
    fn every_place_of_a_text_in_many_scripts() {
        let text = "Привет, мир — «слово» 你好 🎉👍🏽 é\n\tfn main() { let ñ = \"строка\"; }\n\n// комментарий 🎉\nконец";
        let rope = Rope::from(text);
        let before = seen(&text.replace("слово", "дело"), Vec::new());
        for cursor in text.char_indices().map(|(at, _)| at).chain([text.len()]) {
            for format in [Format::Sweep, Format::Zeta] {
                let prompt = build(&Ask { format, path: "a.rs", root: "r", text: &rope, cursor, seen: &before, with_context: true });
                let reply = text.replace("мир", "мир 🎉").replace("строка", "текст");
                if let Some(answer) = parse(&prompt, &rope, &reply) {
                    assert!(text.is_char_boundary(answer.range.start) && text.is_char_boundary(answer.range.end));
                    let _ = prediction(&rope, cursor, answer);
                }
            }
        }
    }

    #[test]
    fn edits_a_second_apart_are_separate_changes() {
        let path: Arc<str> = "src/a.rs".into();
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        let v0 = Rope::from("fn a() {}\n\nfn b() {}\n");
        let v1 = Rope::from("fn a(x: u32) {}\n\nfn b() {}\n");
        let v2 = Rope::from("fn a(x: u32) {}\n\nfn b(x) {}\n");
        let v3 = Rope::from("fn a(x: u32) {}\n\nfn b(x: u32) {}\n");
        let s = tracker.see(&path, &v1, 0, Some(v0.clone()), t0);
        assert!(s.hunks.is_empty() && s.before == v0);
        // Within a second: still the same change.
        let s = tracker.see(&path, &v2, 2, None, t0 + Duration::from_millis(400));
        assert!(s.hunks.is_empty() && s.before == v0);
        // After a pause: what came before is history, the new one is current.
        let s = tracker.see(&path, &v3, 2, None, t0 + Duration::from_millis(2000));
        assert_eq!(s.before, v2);
        let lines: Vec<_> = s.hunks.iter().flat_map(|h| h.lines.iter().filter(|(t, _)| *t != ' ').cloned()).collect();
        assert_eq!(lines, [('-', "fn a() {}".into()), ('+', "fn a(x: u32) {}".into()), ('-', "fn b() {}".into()), ('+', "fn b(x) {}".into())]);
        assert_eq!(s.hunks[0].unified("p"), "--- a/p/src/a.rs\n+++ b/p/src/a.rs\n@@ -1,3 +1,3 @@\n-fn a() {}\n+fn a(x: u32) {}\n \n-fn b() {}\n+fn b(x) {}\n");
    }

    #[test]
    fn history_is_bounded_and_crosses_files() {
        let mut tracker = Tracker::default();
        let t0 = Instant::now();
        for i in 0..10u64 {
            let path: Arc<str> = format!("f{i}.rs").into();
            let text = Rope::from(format!("{}\n", "x".repeat(i as usize + 1)).as_str());
            tracker.see(&path, &text, 0, Some(Rope::from("\n")), t0 + Duration::from_secs(i * 2));
        }
        let s = tracker.see(&"g.rs".into(), &Rope::from(""), 0, None, t0 + Duration::from_secs(60));
        assert_eq!(s.hunks.len(), MAX_HUNKS);
        assert_eq!(&*s.hunks[MAX_HUNKS - 1].path, "f9.rs");
        assert!(tracker.files.len() <= MAX_FILES);
    }

    #[test]
    fn a_big_change_is_left_out() {
        let a = Rope::from("a\n");
        let b = Rope::from("x\n".repeat(200).as_str());
        assert!(diff(&a, &b, &"p".into()).is_empty());
    }

    const FILE: &str = "use std::fmt;\n\nstruct Point {\n    x: f64,\n    y: f64,\n}\n\nimpl Point {\n    fn new(x: f64, y: f64) -> Self {\n        \n    }\n}\n";

    #[test]
    fn sweep_prompt_has_the_original_current_and_updated_window() {
        let text = Rope::from(FILE);
        let cursor = FILE.find("        \n").unwrap() + 8;
        let before = FILE.replace("    fn new(x: f64, y: f64) -> Self {\n        \n    }\n", "");
        let hunk = Hunk { path: "src/other.rs".into(), old_line: 0, new_line: 0, lines: vec![(' ', "a".into()), ('-', "b".into()), ('+', "c".into())] };
        let seen = seen(&before, vec![hunk]);
        let ask = Ask { format: Format::Sweep, path: "src/point.rs", root: "p", text: &text, cursor, seen: &seen, with_context: false };
        let prompt = build(&ask);
        let expected = format!(
            "<|file_sep|>src/other.rs.diff\noriginal:\na\nb\nupdated:\na\nc\n<|file_sep|>original/src/point.rs\n{}\n<|file_sep|>current/src/point.rs\n{}\n<|file_sep|>updated/src/point.rs\n",
            before.trim_end(),
            FILE.trim_end()
        );
        assert_eq!(prompt.text, expected);
        assert_eq!(prompt.region, 0..FILE.len());
        let with = build(&Ask { with_context: true, ..ask });
        assert!(with.text.starts_with("<|file_sep|>src/point.rs\nuse std::fmt;\n"));
    }

    #[test]
    fn sweep_answer_inserted_at_the_cursor_is_ghost_text() {
        let text = Rope::from(FILE);
        let cursor = FILE.find("        \n").unwrap() + 8;
        let seen = seen(FILE, vec![]);
        let prompt = build(&Ask { format: Format::Sweep, path: "a.rs", root: "p", text: &text, cursor, seen: &seen, with_context: false });
        let answer = FILE.replace("        \n", "        Self { x, y }\n").trim_end().to_string() + "<|file_sep|>";
        let edit = parse(&prompt, &text, &answer).unwrap();
        assert_eq!(suggestion(&text, cursor, edit), Some(Suggestion { insert: "Self { x, y }".into(), replaces_to: None }));
    }

    #[test]
    fn new_lines_after_the_cursor_line_slide_to_the_cursor() {
        let file = "fn main() {\n    let a = 1;\n}\n";
        let text = Rope::from(file);
        let cursor = file.find(";").unwrap() + 1;
        let new = "fn main() {\n    let a = 1;\n    let b = 2;\n    println!(\"{a} {b}\");\n}\n";
        let edit = answer(0..file.len(), new.to_string());
        assert_eq!(
            suggestion(&text, cursor, edit),
            Some(Suggestion { insert: "\n    let b = 2;\n    println!(\"{a} {b}\");".into(), replaces_to: None })
        );
    }

    #[test]
    fn a_change_inside_the_line_replaces_its_rest() {
        let file = "call(\n";
        let text = Rope::from(file);
        let cursor = 5;
        let edit = answer(0..file.len(), "call(a, b)\n".to_string());
        assert_eq!(suggestion(&text, cursor, edit), Some(Suggestion { insert: "a, b)".into(), replaces_to: None }));
        let file = "call()\n";
        let text = Rope::from(file);
        let edit = answer(0..file.len(), "call(a, b);\n".to_string());
        assert_eq!(suggestion(&text, 5, edit), Some(Suggestion { insert: "a, b);".into(), replaces_to: Some(6) }));
    }

    #[test]
    fn changes_elsewhere_are_left_and_the_one_at_the_cursor_kept() {
        let file = "let a = 1;\nlet b = 2;\nlet c\n";
        let text = Rope::from(file);
        let cursor = file.len() - 1;
        // The cursor's line completed, and the first line changed too.
        let edit = answer(0..file.len(), "let a = 10;\nlet b = 2;\nlet c = 3;\n".to_string());
        assert_eq!(suggestion(&text, cursor, edit), Some(Suggestion { insert: " = 3;".into(), replaces_to: None }));
        // Only elsewhere: nothing to show.
        let edit = answer(0..file.len(), "let a = 10;\nlet b = 2;\nlet c\n".to_string());
        assert_eq!(suggestion(&text, cursor, edit), None);
        // Before the cursor on its line: not as typed text.
        let edit = answer(0..file.len(), "let a = 1;\nlet b = 2;\nconst c\n".to_string());
        assert_eq!(suggestion(&text, cursor, edit), None);
        // The same text: nothing.
        assert_eq!(suggestion(&text, cursor, answer(0..file.len(), file.to_string())), None);
    }

    #[test]
    fn zeta_prompt_marks_blocks_and_the_cursor() {
        let text = Rope::from(FILE);
        let cursor = FILE.find("        \n").unwrap() + 8;
        let mut before = FILE.to_string();
        before.replace_range(cursor - 4..cursor, "");
        let seen = seen(&before, vec![]);
        let prompt = build(&Ask { format: Format::Zeta, path: "src/point.rs", root: "proj", text: &text, cursor, seen: &seen, with_context: true });
        assert!(prompt.text.starts_with("<[fim-suffix]>\n<[fim-prefix]><filename>proj/src/point.rs\n...\nstruct Point {\n...\nimpl Point {\n    fn new(x: f64, y: f64) -> Self {\n...\n\n"));
        assert!(prompt.text.contains("<filename>edit_history\n--- a/proj/src/point.rs\n+++ b/proj/src/point.rs\n@@ -7,6 +7,6 @@\n"), "{}", prompt.text);
        assert!(prompt.text.ends_with(&format!("<filename>src/point.rs\n<|marker_1|>{}<|user_cursor|>\n    }}\n}}\n<|marker_2|>\n<[fim-middle]>", &FILE[..cursor])), "{}", prompt.text);
        assert_eq!(prompt.region, 0..FILE.len());
        let answer = format!("<|marker_1|>{}Self {{ x, y }}\n    }}\n}}\n<|marker_2|><[end▁of▁sentence]>", &FILE[..cursor]);
        let edit = parse(&prompt, &text, &answer).unwrap();
        assert_eq!(suggestion(&text, cursor, edit), Some(Suggestion { insert: "Self { x, y }".into(), replaces_to: None }));
        // The same marker twice: no change.
        assert!(parse(&prompt, &text, "<|marker_1|><|marker_1|>").is_none());
    }

    fn edits(file: &str, cursor: usize, new: &str) -> Option<Prediction> {
        prediction(&Rope::from(file), cursor, answer(0..file.len(), new.to_string()))
    }

    #[test]
    fn a_change_before_the_cursor_on_its_line_is_shown_where_it_is() {
        let file = "let c\n";
        let p = edits(file, 5, "const c = 3;\n").unwrap();
        assert_eq!(p.ghost, None);
        assert_eq!(p.edits, vec![(0..3, "const".to_string()), (5..5, " = 3;".to_string())]);
    }

    #[test]
    fn a_change_further_on_the_cursor_line_is_shown_where_it_is() {
        let file = "    let email = user.get_name();\n";
        let cursor = file.find(" =").unwrap();
        let p = edits(file, cursor, &file.replace("get_name", "get_email")).unwrap();
        let at = file.find("get_name").unwrap();
        assert_eq!((p.ghost, p.edits), (None, vec![(at..at + 8, "get_email".to_string())]));
    }

    #[test]
    fn a_rename_goes_on_to_the_other_uses_word_by_word() {
        let file = "fn f(count: u32) -> u32 {\n    let y = x + 1;\n    y * x\n}\n";
        let cursor = file.find(':').unwrap();
        let p = edits(file, cursor, &file.replace(" x", " count")).unwrap();
        assert_eq!(p.ghost, None);
        let first = file.find(" x").unwrap() + 1;
        let second = file.rfind(" x").unwrap() + 1;
        assert_eq!(p.edits, vec![(first..first + 1, "count".to_string()), (second..second + 1, "count".to_string())]);
    }

    #[test]
    fn a_new_field_reaches_the_constructor() {
        let file = "struct P {\n    x: f64,\n    y: f64,\n    z: f64,\n}\n\nfn new() -> P {\n    P { x: 0.0, y: 0.0 }\n}\n";
        let cursor = file.find("z: f64,").unwrap() + 7;
        let p = edits(file, cursor, &file.replace("y: 0.0 }", "y: 0.0, z: 0.0 }")).unwrap();
        let at = file.find("y: 0.0").unwrap() + 6;
        assert_eq!((p.ghost, p.edits), (None, vec![(at..at, ", z: 0.0".to_string())]));
    }

    #[test]
    fn whole_lines_inserted_elsewhere_go_after_the_line_before() {
        let p = edits("a\nb\nc\n", 1, "a\nb\nX\nc\n").unwrap();
        assert_eq!(p.edits, vec![(3..3, "\nX".to_string())]);
        let p = edits("fn a() {\n    b();\n}\n", 8, "fn a() {\n    b();\n    c();\n}\n").unwrap();
        assert_eq!(p.edits, vec![(17..17, "\n    c();".to_string())], "{p:?}");
    }

    #[test]
    fn the_ghost_is_one_of_the_edits_and_blank_changes_are_left() {
        let file = "let a = 1;\nlet b = 2;\nlet c\n";
        let cursor = file.len() - 1;
        let p = edits(file, cursor, "let a = 10;\n\nlet b = 2;  \nlet c = 3;\n").unwrap();
        assert_eq!(p.ghost, Some(Suggestion { insert: " = 3;".into(), replaces_to: None }));
        assert_eq!(p.edits, vec![(9..9, "0".to_string()), (cursor..cursor, " = 3;".to_string())]);
        assert_eq!(edits(file, cursor, "let a = 1;\n\nlet b = 2;\nlet c\n"), None);
    }

    #[test]
    fn many_changes_in_a_few_lines_are_one() {
        let file = "a b c d e f g h i j k l m n\nx\n";
        let p = edits(file, file.len() - 1, "1 b 2 d 3 f 4 h 5 j 6 l 7 n\nx\n").unwrap();
        assert_eq!(p.edits, vec![(0..25, "1 b 2 d 3 f 4 h 5 j 6 l 7".to_string())]);
    }

    #[test]
    fn zeta_says_where_the_caret_goes() {
        let text = Rope::from(FILE);
        let cursor = FILE.find("        \n").unwrap() + 8;
        let seen = seen(FILE, vec![]);
        let prompt = build(&Ask { format: Format::Zeta, path: "a.rs", root: "p", text: &text, cursor, seen: &seen, with_context: false });
        let new = FILE.replace("x: f64,\n    y", "x: f32,\n    y").replace("        \n", "        Self { x, y }<|user_cursor|>\n");
        let output = format!("<|marker_1|>{new}<|marker_2|><[end▁of▁sentence]>");
        let answer = parse(&prompt, &text, &output).unwrap();
        let caret = FILE.find("        \n").unwrap() + 8 + "Self { x, y }".len();
        assert_eq!(answer.caret, Some(caret));
        let p = prediction(&text, cursor, answer).unwrap();
        assert_eq!(p.caret, Some(caret));
        let at = FILE.find("x: f64").unwrap() + 3;
        assert_eq!(p.edits, vec![(at..at + 3, "f32".to_string()), (cursor..cursor, "Self { x, y }".to_string())]);
    }

    #[test]
    fn zeta_markers_split_long_text_at_blank_lines() {
        let block = "a\nb\nc\nd\ne\nf\ng\n";
        let text = format!("{block}\n{block}\n{block}");
        let offs = markers(&text);
        assert_eq!(offs, vec![0, block.len() + 1, 2 * (block.len() + 1), text.len()]);
        assert_eq!(markers(""), vec![0, 0]);
        assert_eq!(marker_tags("x<|marker_12|>y<|marker_|>"), vec![(1, 14, 12)]);
    }
}
