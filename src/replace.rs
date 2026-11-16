//! Replace in files: every match of a query under the task's worktree with
//! what it becomes, those chosen written back. Open files are edited in
//! their tab, the others on disk, each only if it is still what was searched.

use std::{
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use grep_matcher::{Captures as _, Matcher as _};
use grep_regex::RegexMatcher;
use ignore::{WalkBuilder, WalkState};

/// More than this many matches are not looked for: the list says so.
pub const LIMIT: usize = 2000;
/// A file this large is not read whole to be rewritten.
const MAX_FILE: u64 = 16 << 20;

#[derive(Clone, Default)]
pub struct Query {
    pub text: String,
    pub regex: bool,
    /// Globs as Find in files takes them.
    pub mask: String,
    /// With `regex`, `$1` and `${name}` are the groups, `$$` a dollar.
    pub replacement: String,
}

/// A text without the text: its length and a hash, to tell later whether a
/// file is still as it was searched.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fingerprint {
    len: usize,
    hash: u64,
}

impl Fingerprint {
    pub fn of(bytes: &[u8]) -> Self {
        Self::of_chunks(std::iter::once(bytes))
    }

    pub fn of_chunks<'a>(chunks: impl Iterator<Item = &'a [u8]>) -> Self {
        let (mut len, mut hash) = (0, 0xcbf2_9ce4_8422_2325u64);
        for chunk in chunks {
            len += chunk.len();
            hash = chunk.iter().fold(hash, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3));
        }
        Self { len, hash }
    }
}

pub struct Hit {
    /// Where in the file, in bytes.
    pub range: Range<usize>,
    /// 1-based.
    pub line: u32,
    pub replacement: String,
    /// A piece of the line with both the old text and the new one in it,
    /// and where each is.
    pub preview: String,
    pub old: Range<usize>,
    pub new: Range<usize>,
}

pub struct FileHits {
    /// Relative to the root, with `/`.
    pub path: String,
    pub print: Fingerprint,
    pub hits: Vec<Hit>,
}

#[derive(Default)]
pub struct Found {
    pub files: Vec<FileHits>,
    /// More matches were there than `LIMIT`.
    pub truncated: bool,
    pub error: Option<String>,
}

impl Found {
    pub fn count(&self) -> usize {
        self.files.iter().map(|f| f.hits.len()).sum()
    }
}

/// The matcher Find in files would use for the query: smart case.
pub fn matcher(query: &Query) -> Result<RegexMatcher, String> {
    let made = if query.regex { crate::search::regex(&query.text) } else { crate::search::literal(&query.text) };
    made.ok_or_else(|| if query.regex { "Not a regular expression".to_string() } else { "Cannot be searched for".to_string() })
}

/// The matches in `text`, a line at a time as rg does, with their
/// replacements; at most `limit`.
pub fn find_in_text(text: &str, matcher: &RegexMatcher, query: &Query, limit: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    let Ok(mut caps) = matcher.new_captures() else { return hits };
    let mut start = 0;
    for (ix, raw) in text.split_inclusive('\n').enumerate() {
        if hits.len() >= limit {
            break;
        }
        let line = raw.trim_end_matches(['\n', '\r']);
        let bytes = line.as_bytes();
        let _ = matcher.captures_iter(bytes, &mut caps, |caps| {
            let Some(m) = caps.get(0) else { return true };
            if m.is_empty() || !line.is_char_boundary(m.start()) || !line.is_char_boundary(m.end()) {
                return true;
            }
            let replacement = if query.regex {
                let mut dst = Vec::new();
                caps.interpolate(|name| matcher.capture_index(name), bytes, query.replacement.as_bytes(), &mut dst);
                String::from_utf8_lossy(&dst).into_owned()
            } else {
                query.replacement.clone()
            };
            let (preview, old, new) = preview(line, m.start()..m.end(), &replacement);
            hits.push(Hit { range: start + m.start()..start + m.end(), line: ix as u32 + 1, replacement, preview, old, new });
            hits.len() < limit
        });
        start += raw.len();
    }
    hits
}

/// The line about the match: the indent and what is far before it left out,
/// then the old text, the new one, and what follows.
fn preview(line: &str, at: Range<usize>, replacement: &str) -> (String, Range<usize>, Range<usize>) {
    const BEFORE: usize = 40;
    const AFTER: usize = 80;
    let before = line[..at.start].trim_start();
    let mut lead = String::new();
    let count = before.chars().count();
    if count > BEFORE {
        lead.push('…');
        lead.extend(before.chars().skip(count - BEFORE));
    } else {
        lead.push_str(before);
    }
    let old = lead.len()..lead.len() + at.len();
    let mut out = lead;
    out.push_str(&line[at.clone()]);
    let new = out.len()..out.len() + replacement.len();
    out.push_str(replacement);
    let after = &line[at.end..];
    out.extend(after.chars().take(AFTER));
    if after.chars().nth(AFTER).is_some() {
        out.push('…');
    }
    (out, old, new)
}

/// The matches under `root`, files as Find in files walks them (.gitignore,
/// no hidden files, no binary ones), sorted by path; at most `limit`.
pub fn find(root: &Path, query: &Query, limit: usize, stop: &AtomicBool) -> Found {
    let matcher = match matcher(query) {
        Ok(matcher) => matcher,
        Err(error) => return Found { error: Some(error), ..Found::default() },
    };
    let found = Mutex::new(Vec::new());
    let count = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);
    let mut walk = WalkBuilder::new(root);
    walk.require_git(false);
    if let Some(mask) = crate::search::mask(root, &query.mask) {
        walk.overrides(mask);
    }
    walk.build_parallel().run(|| {
        let (found, count, truncated, matcher) = (&found, &count, &truncated, &matcher);
        Box::new(move |entry| {
            if stop.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            if count.load(Ordering::Relaxed) >= limit {
                truncated.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let Ok(entry) = entry else { return WalkState::Continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) || entry.metadata().is_ok_and(|m| m.len() > MAX_FILE) {
                return WalkState::Continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else { return WalkState::Continue };
            if !matcher.is_match(&bytes).unwrap_or(false) || bytes.contains(&0) {
                return WalkState::Continue;
            }
            // Only text: what is not UTF-8 would not be written back as it was.
            let Ok(text) = std::str::from_utf8(&bytes) else { return WalkState::Continue };
            let hits = find_in_text(text, matcher, query, limit);
            if hits.is_empty() {
                return WalkState::Continue;
            }
            count.fetch_add(hits.len(), Ordering::Relaxed);
            let path = entry.path().strip_prefix(root).unwrap_or(entry.path());
            let path = path.to_string_lossy().replace('\\', "/");
            found.lock().unwrap().push(FileHits { path, print: Fingerprint::of(&bytes), hits });
            WalkState::Continue
        })
    });
    let mut files = found.into_inner().unwrap();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut left = limit;
    let mut cut = truncated.into_inner();
    files.retain_mut(|file| {
        if left == 0 {
            cut = true;
            return false;
        }
        if file.hits.len() > left {
            file.hits.truncate(left);
            cut = true;
        }
        left -= file.hits.len();
        true
    });
    Found { files, truncated: cut, error: None }
}

/// `text` with each range replaced; the ranges in order, not overlapping.
pub fn apply(text: &str, edits: &[(Range<usize>, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (range, with) in edits {
        let Some(kept) = text.get(at..range.start) else { continue };
        if text.get(range.clone()).is_none() {
            continue;
        }
        out.push_str(kept);
        out.push_str(with);
        at = range.end;
    }
    out.push_str(&text[at..]);
    out
}

/// What to do to one file: it must still be `print`.
pub struct Plan {
    pub path: PathBuf,
    pub print: Fingerprint,
    pub edits: Vec<(Range<usize>, String)>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Replaced(usize),
    /// Not as it was searched: someone wrote it since.
    Changed,
    /// Open in a tab with edits not saved.
    Unsaved,
    Failed(String),
}

/// Off the UI thread: a file not open, rewritten if it is still as searched.
pub fn write(plan: &Plan) -> Outcome {
    let bytes = match std::fs::read(&plan.path) {
        Ok(bytes) => bytes,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    if Fingerprint::of(&bytes) != plan.print {
        return Outcome::Changed;
    }
    let Ok(text) = String::from_utf8(bytes) else { return Outcome::Changed };
    match std::fs::write(&plan.path, apply(&text, &plan.edits)) {
        Ok(()) => Outcome::Replaced(plan.edits.len()),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// What a replace did, file by file (relative paths).
#[derive(Default)]
pub struct Report {
    pub done: Vec<(String, Outcome)>,
}

impl Report {
    pub fn replaced(&self) -> (usize, usize) {
        let counts = self.done.iter().filter_map(|(_, o)| match o {
            Outcome::Replaced(n) => Some(*n),
            _ => None,
        });
        counts.fold((0, 0), |(matches, files), n| (matches + n, files + 1))
    }

    pub fn message(&self) -> String {
        let (matches, files) = self.replaced();
        let mut out = match (matches, files) {
            (0, _) => "Nothing replaced.".to_string(),
            (1, _) => "Replaced 1 match.".to_string(),
            (m, 1) => format!("Replaced {m} matches in 1 file."),
            (m, f) => format!("Replaced {m} matches in {f} files."),
        };
        let list = |pick: fn(&Outcome) -> bool| {
            let names: Vec<&str> = self.done.iter().filter(|(_, o)| pick(o)).map(|(p, _)| p.as_str()).collect();
            match names.len() {
                0 => None,
                n if n > 5 => Some(format!("{} and {} more", names[..5].join(", "), n - 5)),
                _ => Some(names.join(", ")),
            }
        };
        if let Some(names) = list(|o| *o == Outcome::Unsaved) {
            out.push_str(&format!("\nSkipped, unsaved edits in a tab: {names}. Save and replace again."));
        }
        if let Some(names) = list(|o| *o == Outcome::Changed) {
            out.push_str(&format!("\nSkipped, changed since the search: {names}."));
        }
        for (path, outcome) in &self.done {
            if let Outcome::Failed(why) = outcome {
                out.push_str(&format!("\n{path}: {why}"));
            }
        }
        out
    }
}

// ---------------------------------------------------------------- dialog

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use crate::task::TaskView;

#[derive(Clone, Copy)]
enum Row {
    File(usize),
    Hit(usize, usize),
}

pub struct ReplaceView {
    root: PathBuf,
    task: WeakEntity<TaskView>,
    query: Entity<InputState>,
    replacement: Entity<InputState>,
    mask: Entity<InputState>,
    regex: bool,
    found: Rc<Found>,
    rows: Rc<Vec<Row>>,
    off_files: HashSet<usize>,
    off_hits: HashSet<(usize, usize)>,
    /// Of the files found, those open in a tab with edits not saved.
    unsaved: HashSet<usize>,
    searching: bool,
    applying: bool,
    scroll: UniformListScrollHandle,
    _search: Task<()>,
    _apply: Task<()>,
}

/// Opens the dialog over the task's worktree.
pub fn open(root: PathBuf, task: WeakEntity<TaskView>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| ReplaceView::new(root, task, window, cx));
    show(view, window, cx);
}

fn show(view: Entity<ReplaceView>, window: &mut Window, cx: &mut App) {
    let focused = Rc::new(std::cell::Cell::new(false));
    window.open_dialog(cx, move |dialog, _, _| {
        let (view, focused) = (view.clone(), focused.clone());
        dialog.title("Replace in files").w(px(900.)).content(move |content, window, cx| {
            if !focused.replace(true) {
                let query = view.read(cx).query.clone();
                window.defer(cx, move |window, cx| query.read(cx).focus_handle(cx).focus(window, cx));
            }
            content.child(view.clone())
        })
    });
}

impl ReplaceView {
    fn new(root: PathBuf, task: WeakEntity<TaskView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Find"));
        let replacement = cx.new(|cx| InputState::new(window, cx).placeholder("Replace with"));
        let mask = cx.new(|cx| InputState::new(window, cx).placeholder("*.rs, src/**, !*_test.go"));
        for input in [&query, &replacement, &mask] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.search(cx);
                }
            })
            .detach();
        }
        Self {
            root,
            task,
            query,
            replacement,
            mask,
            regex: false,
            found: Rc::default(),
            rows: Rc::default(),
            off_files: HashSet::new(),
            off_hits: HashSet::new(),
            unsaved: HashSet::new(),
            searching: false,
            applying: false,
            scroll: UniformListScrollHandle::new(),
            _search: Task::ready(()),
            _apply: Task::ready(()),
        }
    }

    fn query(&self, cx: &App) -> Query {
        Query {
            text: self.query.read(cx).value().to_string(),
            regex: self.regex,
            mask: self.mask.read(cx).value().to_string(),
            replacement: self.replacement.read(cx).value().to_string(),
        }
    }

    /// After typing pauses, off the UI thread; a newer search replaces it.
    fn search(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        if query.text.trim().is_empty() {
            self._search = Task::ready(());
            self.searching = false;
            self.set_found(Found::default(), cx);
            return;
        }
        self.searching = true;
        let root = self.root.clone();
        self._search = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let stop = crate::search::StopOnDrop::default();
            let flag = stop.flag();
            let found = cx.background_spawn(async move { find(&root, &query, LIMIT, &flag) }).await;
            drop(stop);
            let _ = this.update(cx, |this, cx| {
                this.searching = false;
                this.set_found(found, cx);
            });
        });
        cx.notify();
    }

    fn set_found(&mut self, found: Found, cx: &mut Context<Self>) {
        let unsaved = self.task.upgrade().map(|t| t.read(cx).unsaved_paths(cx)).unwrap_or_default();
        self.unsaved = found.files.iter().enumerate().filter(|(_, f)| unsaved.contains(&self.root.join(&f.path))).map(|(ix, _)| ix).collect();
        let mut rows = Vec::with_capacity(found.count() + found.files.len());
        for (fx, file) in found.files.iter().enumerate() {
            rows.push(Row::File(fx));
            rows.extend((0..file.hits.len()).map(|hx| Row::Hit(fx, hx)));
        }
        self.rows = Rc::new(rows);
        self.found = Rc::new(found);
        self.off_files.clear();
        self.off_hits.clear();
        cx.notify();
    }

    fn chosen(&self, fx: usize, hx: usize) -> bool {
        !self.off_files.contains(&fx) && !self.off_hits.contains(&(fx, hx))
    }

    fn chosen_count(&self) -> usize {
        let found = &self.found;
        (0..found.files.len())
            .filter(|fx| !self.unsaved.contains(fx))
            .map(|fx| (0..found.files[fx].hits.len()).filter(|&hx| self.chosen(fx, hx)).count())
            .sum()
    }

    fn plans(&self, only: Option<usize>) -> Vec<(String, Plan)> {
        self.found
            .files
            .iter()
            .enumerate()
            .filter(|(fx, _)| only.is_none_or(|only| only == *fx))
            .filter_map(|(fx, file)| {
                let edits: Vec<_> = file
                    .hits
                    .iter()
                    .enumerate()
                    .filter(|(hx, _)| self.chosen(fx, *hx))
                    .map(|(_, hit)| (hit.range.clone(), hit.replacement.clone()))
                    .collect();
                (!edits.is_empty()).then(|| (file.path.clone(), Plan { path: self.root.join(&file.path), print: file.print, edits }))
            })
            .collect()
    }

    /// All chosen, or those of one file: the dialog closes after all, one
    /// file leaves the list.
    fn replace(&mut self, only: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let plans = self.plans(only);
        let Some(task) = self.task.upgrade() else { return };
        if plans.is_empty() || self.applying {
            return;
        }
        self.applying = true;
        let running = task.update(cx, |t, cx| t.replace_in_files(plans, window, cx));
        self._apply = cx.spawn_in(window, async move |this, cx| {
            let report = running.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.applying = false;
                window.push_notification(Notification::info(report.message()).title("Replace in files"), cx);
                match only {
                    None => window.close_dialog(cx),
                    Some(fx) => {
                        let replaced = report.done.iter().any(|(_, o)| matches!(o, Outcome::Replaced(_)));
                        if replaced {
                            this.drop_file(fx, cx);
                        }
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn drop_file(&mut self, fx: usize, cx: &mut Context<Self>) {
        let Some(mut found) = Rc::into_inner(std::mem::take(&mut self.found)) else { return self.search(cx) };
        found.files.remove(fx);
        let shift = |ix: usize| (ix != fx).then_some(if ix > fx { ix - 1 } else { ix });
        let off_hits: HashSet<_> = self.off_hits.iter().filter_map(|&(f, h)| Some((shift(f)?, h))).collect();
        let off_files: HashSet<_> = self.off_files.iter().filter_map(|&f| shift(f)).collect();
        self.set_found(found, cx);
        (self.off_hits, self.off_files) = (off_hits, off_files);
    }

    fn toggle_file(&mut self, fx: usize, cx: &mut Context<Self>) {
        if !self.off_files.remove(&fx) {
            self.off_files.insert(fx);
        }
        self.off_hits.retain(|(f, _)| *f != fx);
        cx.notify();
    }

    fn toggle_hit(&mut self, fx: usize, hx: usize, cx: &mut Context<Self>) {
        if self.off_files.remove(&fx) {
            // The file was left out: now all of it but this one counts.
            let count = self.found.files.get(fx).map_or(0, |f| f.hits.len());
            self.off_hits.extend((0..count).filter(|&h| h != hx).map(|h| (fx, h)));
        } else if !self.off_hits.remove(&(fx, hx)) {
            self.off_hits.insert((fx, hx));
        }
        cx.notify();
    }

    fn status(&self) -> String {
        let found = &self.found;
        if let Some(error) = &found.error {
            return error.clone();
        }
        if self.searching {
            return "Searching…".to_string();
        }
        let count = found.count();
        let mut out = match (count, found.files.len()) {
            (0, _) => String::new(),
            (1, _) => "1 match".to_string(),
            (n, 1) => format!("{n} matches in 1 file"),
            (n, f) => format!("{n} matches in {f} files"),
        };
        if found.truncated {
            out.push_str(&format!(" — only the first {LIMIT} are shown and replaced; run it again for the rest"));
        }
        if !self.unsaved.is_empty() {
            out.push_str(" — files with unsaved edits in a tab are skipped");
        }
        out
    }

    fn render_row(&self, row: Row, entity: &Entity<Self>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let found = &self.found;
        match row {
            Row::File(fx) => {
                let file = &found.files[fx];
                let (dir, name) = file.path.rsplit_once('/').map_or(("", file.path.as_str()), |(d, n)| (d, n));
                let unsaved = self.unsaved.contains(&fx);
                let (toggle, replace) = (entity.clone(), entity.clone());
                h_flex()
                    .id(("replace-file", fx))
                    .h(px(26.))
                    .px_2()
                    .gap_2()
                    .w_full()
                    .child(
                        Checkbox::new(("replace-file-check", fx))
                            .checked(!self.off_files.contains(&fx) && !unsaved)
                            .disabled(unsaved)
                            .on_click(move |_, _, cx| toggle.update(cx, |this, cx| this.toggle_file(fx, cx))),
                    )
                    .child(div().flex_none().text_sm().font_weight(FontWeight::SEMIBOLD).child(name.to_string()))
                    .child(div().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(dir.to_string()))
                    .child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(file.hits.len().to_string()))
                    .when(unsaved, |row| row.child(div().flex_none().text_xs().text_color(theme.warning).child("unsaved in a tab: skipped")))
                    .child(div().flex_1())
                    .when(!unsaved, |row| {
                        row.child(
                            Button::new(("replace-in-file", fx))
                                .xsmall()
                                .ghost()
                                .label("Replace")
                                .disabled(self.applying || self.searching)
                                .tooltip("Replace the chosen matches of this file")
                                .on_click(move |_, window, cx| replace.update(cx, |this, cx| this.replace(Some(fx), window, cx))),
                        )
                    })
                    .into_any_element()
            }
            Row::Hit(fx, hx) => {
                let hit = &found.files[fx].hits[hx];
                let unsaved = self.unsaved.contains(&fx);
                let toggle = entity.clone();
                let old = HighlightStyle {
                    background_color: Some(hsla(0.0, 0.8, 0.55, 0.25)),
                    strikethrough: Some(StrikethroughStyle { thickness: px(1.), color: None }),
                    ..Default::default()
                };
                let new = HighlightStyle { background_color: Some(hsla(0.33, 0.7, 0.45, 0.3)), font_weight: Some(FontWeight::BOLD), ..Default::default() };
                let mut highlights = vec![(hit.old.clone(), old)];
                if !hit.new.is_empty() {
                    highlights.push((hit.new.clone(), new));
                }
                h_flex()
                    .id(("replace-hit", fx * 100_000 + hx))
                    .h(px(26.))
                    .pl_8()
                    .pr_2()
                    .gap_2()
                    .w_full()
                    .child(
                        Checkbox::new(("replace-hit-check", fx * 100_000 + hx))
                            .checked(self.chosen(fx, hx) && !unsaved)
                            .disabled(unsaved)
                            .on_click(move |_, _, cx| toggle.update(cx, |this, cx| this.toggle_hit(fx, hx, cx))),
                    )
                    .child(div().flex_none().w(px(44.)).text_xs().text_color(theme.muted_foreground).child(hit.line.to_string()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.mono_font_family.clone())
                            .text_sm()
                            .child(StyledText::new(hit.preview.clone()).with_highlights(highlights)),
                    )
                    .into_any_element()
            }
        }
    }
}

impl Render for ReplaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let rows = self.rows.clone();
        let chosen = self.chosen_count();
        let status = self.status();
        let list_owner = entity.clone();
        let label = |text: &'static str| div().flex_none().w(px(72.)).text_sm().text_color(cx.theme().muted_foreground).child(text);
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(label("Find"))
                    .child(div().flex_1().child(Input::new(&self.query))),
            )
            .child(h_flex().gap_2().child(label("Replace")).child(div().flex_1().child(Input::new(&self.replacement))))
            .child(
                h_flex()
                    .gap_2()
                    .child(label("Files"))
                    .child(div().w(px(320.)).child(Input::new(&self.mask).small()))
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(cx.theme().muted_foreground).child(status))
                    .child(
                        Button::new("replace-regex")
                            .small()
                            .ghost()
                            .label(".* Regex")
                            .selected(self.regex)
                            .tooltip("The query as a regular expression; $1 in the replacement is its first group")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.regex = !this.regex;
                                this.search(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .h(px(420.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(
                        uniform_list("replace-rows", rows.len(), move |range, _, cx| {
                            let this = list_owner.read(cx);
                            range.map(|ix| this.render_row(rows[ix], &list_owner, cx)).collect()
                        })
                        .size_full()
                        .track_scroll(&self.scroll),
                    ),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("replace-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(
                        Button::new("replace-all")
                            .primary()
                            .label(if chosen == 0 { "Replace all".to_string() } else { format!("Replace {chosen}") })
                            .disabled(chosen == 0 || self.searching || self.applying)
                            .on_click(cx.listener(|this, _, window, cx| this.replace(None, window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::atomic::AtomicBool};

    use super::{Fingerprint, Outcome, Plan, Query, apply, find, find_in_text, matcher, write};

    fn query(text: &str, regex: bool, replacement: &str) -> Query {
        Query { text: text.into(), regex, mask: String::new(), replacement: replacement.into() }
    }

    fn replaced(text: &str, q: &Query) -> String {
        let hits = find_in_text(text, &matcher(q).unwrap(), q, 100);
        apply(text, &hits.iter().map(|h| (h.range.clone(), h.replacement.clone())).collect::<Vec<_>>())
    }

    #[test]
    fn literal_is_not_a_pattern_and_smart_case() {
        let q = query("a.b($1)", false, "x$1");
        assert_eq!(replaced("a.b($1) aXb($1)\nA.B($1)\n", &q), "x$1 aXb($1)\nx$1\n", "a lowercase query ignores case");
        let upper = query("Foo", false, "Bar");
        assert_eq!(replaced("foo Foo\r\nFOO\n", &upper), "foo Bar\r\nFOO\n", "an uppercase query minds case; CRLF kept");
    }

    #[test]
    fn regex_groups_and_lines() {
        let q = query(r"(\w+)\s*=\s*(\d+)", true, "$2 => ${1}");
        assert_eq!(replaced("let a = 1;\nb=22\n", &q), "let 1 => a;\n22 => b\n");
        let dollar = query(r"\$(\w+)", true, "$$$1!");
        assert_eq!(replaced("cost $usd\n", &dollar), "cost $usd!\n");
        let eol = query(r"x$", true, "y");
        assert_eq!(replaced("ax\r\nbx\nx z\n", &eol), "ay\r\nby\nx z\n", "$ is the end of each line");
        assert_eq!(replaced("aaa", &query("b*", true, "-")), "aaa", "empty matches are left alone");

        let hits = find_in_text("    let name = é;\n", &matcher(&query("name", false, "title")).unwrap(), &query("name", false, "title"), 10);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!((hit.line, hit.range.clone()), (1, 8..12));
        assert_eq!(&hit.preview[hit.old.clone()], "name");
        assert_eq!(&hit.preview[hit.new.clone()], "title");
        assert!(hit.preview.starts_with("let "), "no indent: {}", hit.preview);
        assert!(matcher(&query("(", true, "")).is_err());
    }

    #[test]
    fn writes_only_files_as_searched() {
        let root = std::env::temp_dir().join(format!("ideae-replace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("ignored.txt"), "old\n").unwrap();
        fs::write(root.join("bin.dat"), b"old\0").unwrap();
        fs::write(root.join("a.txt"), "old one\nnew\nold two old\n").unwrap();
        fs::write(root.join("b.txt"), "the old\n").unwrap();
        let q = query("old", false, "new");
        let found = find(&root, &q, 100, &AtomicBool::new(false));
        let paths: Vec<_> = found.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt"], "ignored and binary files are left out");
        assert_eq!(found.count(), 4);
        assert!(!found.truncated);
        let capped = find(&root, &q, 2, &AtomicBool::new(false));
        assert!(capped.truncated && capped.count() == 2);

        let plan = |ix: usize, keep: &dyn Fn(usize) -> bool| {
            let file = &found.files[ix];
            let edits = file.hits.iter().enumerate().filter(|(i, _)| keep(*i)).map(|(_, h)| (h.range.clone(), h.replacement.clone())).collect();
            Plan { path: root.join(&file.path), print: file.print, edits }
        };
        // b.txt is written by another after the search.
        fs::write(root.join("b.txt"), "the old one\n").unwrap();
        assert_eq!(write(&plan(1, &|_| true)), Outcome::Changed);
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "the old one\n");
        // The middle match of a.txt left out.
        assert_eq!(write(&plan(0, &|i| i != 1)), Outcome::Replaced(2));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "new one\nnew\nold two new\n");
        assert_eq!(write(&plan(0, &|_| true)), Outcome::Changed, "written once, the print no longer holds");
        assert_eq!(Fingerprint::of_chunks([&b"ab"[..], b"c"].into_iter()), Fingerprint::of(b"abc"));
        fs::remove_dir_all(&root).unwrap();
    }

    mod ui {
        use std::{fs, time::Duration};

        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{AppContext as _, Bounds, Empty, Point, TestAppContext, WeakEntity, WindowBounds, WindowOptions, px, size};

        use super::super::{ReplaceView, show};

        #[gpui_kit::test]
        fn matches_by_file_some_left_out(cx: &mut TestAppContext) {
            cx.update(gpui_kit::init);
            let root = std::env::temp_dir().join(format!("ideae-replace-ui-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("a.txt"), "old one\nold two\n").unwrap();
            fs::write(root.join("b.txt"), "the old\n").unwrap();
            let handle = cx.update(|cx| {
                let bounds = Bounds { origin: Point::default(), size: size(px(1200.), px(800.)) };
                let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
                gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| Empty)).expect("a test window").0
            });
            let view = cx
                .update_window(handle, |_, window, cx| {
                    let view = cx.new(|cx| ReplaceView::new(root.clone(), WeakEntity::new_invalid(), window, cx));
                    show(view.clone(), window, cx);
                    let (query, replacement) = (view.read(cx).query.clone(), view.read(cx).replacement.clone());
                    query.update(cx, |input, cx| input.set_value("old", window, cx));
                    replacement.update(cx, |input, cx| input.set_value("new", window, cx));
                    view.update(cx, |view, cx| view.search(cx));
                    view
                })
                .unwrap();
            cx.executor().advance_clock(Duration::from_millis(300));
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                assert_eq!(view.read(cx).rows.len(), 5, "two files, three matches");
                assert_eq!(view.read(cx).chosen_count(), 3);
                window.render_frame(cx);
                window.click(("replace-hit-check", 1usize), cx);
                assert_eq!(view.read(cx).chosen_count(), 2, "the second match of a.txt left out");
                window.click(("replace-file-check", 1usize), cx);
                assert_eq!(view.read(cx).chosen_count(), 1, "b.txt left out");
                let plans = view.read(cx).plans(None);
                assert_eq!(plans.len(), 1);
                assert_eq!(plans[0].1.edits, [(0..3, "new".to_string())]);
            })
            .unwrap();
            fs::remove_dir_all(&root).unwrap();
        }
    }
}
