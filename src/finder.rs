//! Go to file (fuzzy, over `git ls-files`) and find in files (ripgrep), as a
//! command palette in a dialog.

use std::{path::PathBuf, rc::Rc, sync::Arc, time::Duration};

use gpui_kit::component::{
    ActiveTheme as _, IndexPath, Selectable as _, Sizable as _, StyledExt as _, WindowExt as _,
    button::ButtonVariants as _,
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex, v_flex,
};
use gpui_kit::*;

use crate::{search, task::TaskView};

#[derive(Clone, Copy, PartialEq)]
pub enum FinderKind {
    Files,
    Grep,
    /// The declarations of one file (Ctrl+F12); `files` holds that file.
    Outline,
    /// Places in sections under headings, such as implementations and usages.
    Places,
    /// What the project declares under a name: asked of `symbols` as you type.
    Symbols,
    /// A list to choose one of, such as branches: the choice goes to `picked`.
    Pick,
}

/// What is done with the one chosen from a `Pick` list: given its `path`.
pub type Picked = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// Finds declarations by a part of their name. A hit's text is the name,
/// then `DETAIL` and what it is.
pub type SymbolSource = Arc<dyn Fn(&str) -> Vec<Hit> + Send + Sync>;
pub const DETAIL: char = '\u{1f}';

/// A declaration for the outline: nesting depth, label, 0-based line.
pub struct OutlineItem {
    pub depth: usize,
    pub label: String,
    pub line: u32,
    /// The declaration's last line.
    pub end: u32,
}

#[derive(Clone)]
pub struct Hit {
    pub path: String,
    /// 1-based; 0 for a file hit.
    pub line: u32,
    /// 0-based, in characters.
    pub col: u32,
    pub text: String,
    /// The matches in `text`, in bytes, for Find in files.
    pub ranges: Vec<std::ops::Range<usize>>,
}

pub struct Finder {
    kind: FinderKind,
    /// The whole outline, filtered into `hits` by the query.
    outline: Arc<Vec<Hit>>,
    /// For `Places`: the headings, each with its places; `hits` holds them
    /// all and `shown` how many of each pass the query.
    sections: Arc<Vec<(String, Vec<Hit>)>>,
    shown: Rc<Vec<usize>>,
    root: PathBuf,
    files: Arc<Vec<String>>,
    task: WeakEntity<TaskView>,
    hits: Rc<Vec<Hit>>,
    _search: Task<()>,
    /// Find in files: the query as typed, whether it is a regex, and the
    /// file mask.
    last_query: String,
    regex: bool,
    mask: Option<Entity<gpui_kit::component::input::InputState>>,
    symbols: Option<SymbolSource>,
    picked: Option<(String, Picked)>,
    /// Find in files: a search under way, and what the last one found, told
    /// under the list.
    searching: bool,
    told: SharedString,
}

thread_local! {
    /// The title and the taker of the list being opened: on their way from
    /// `open_pick` into the finder it makes.
    static PICK: std::cell::RefCell<Option<(String, Picked)>> = const { std::cell::RefCell::new(None) };
}

const FILE_LIMIT: usize = 100;
const GREP_LIMIT: usize = 200;

impl Finder {
    pub fn open(
        kind: FinderKind,
        root: PathBuf,
        files: Arc<Vec<String>>,
        task: WeakEntity<TaskView>,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_with(kind, root, files, Vec::new(), task, window, cx)
    }

    /// A fixed list of places to choose from (several definitions of a name).
    pub fn open_hits(root: PathBuf, hits: Vec<Hit>, task: WeakEntity<TaskView>, window: &mut Window, cx: &mut App) {
        Self::open_with(FinderKind::Outline, root, Arc::default(), hits, task, window, cx)
    }

    /// Places under headings (Ctrl+Alt+B: implementations, then usages).
    /// Sections without places are left out.
    pub fn open_places(
        root: PathBuf,
        title: String,
        sections: Vec<(String, Vec<Hit>)>,
        task: WeakEntity<TaskView>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let sections = sections.into_iter().filter(|(_, hits)| !hits.is_empty()).collect();
        Self::open_inner(FinderKind::Places, root, Arc::default(), Vec::new(), sections, title, None, task, window, cx)
    }

    /// The outline of `path`, from items already extracted.
    pub fn open_outline(
        root: PathBuf,
        path: String,
        items: Vec<OutlineItem>,
        task: WeakEntity<TaskView>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let hits = items
            .into_iter()
            .map(|i| {
                let (kind, name) = i.label.rsplit_once(' ').unwrap_or(("", i.label.as_str()));
                let text = format!("{}{name}{DETAIL}{kind}", "  ".repeat(i.depth));
                Hit { path: path.clone(), line: i.line + 1, col: 0, text, ranges: Vec::new() }
            })
            .collect();
        Self::open_with(FinderKind::Outline, root, Arc::default(), hits, task, window, cx)
    }

    /// Go to a class, a function, a type anywhere in the project.
    pub fn open_symbols(root: PathBuf, symbols: SymbolSource, task: WeakEntity<TaskView>, window: &mut Window, cx: &mut App) {
        Self::open_inner(FinderKind::Symbols, root, Arc::default(), Vec::new(), Vec::new(), String::new(), Some(symbols), task, window, cx)
    }

    /// One of `items` to choose, each a name with `DETAIL` and what it is;
    /// `picked` gets the `path` of the one chosen.
    pub fn open_pick(title: String, root: PathBuf, items: Vec<Hit>, picked: Picked, task: WeakEntity<TaskView>, window: &mut Window, cx: &mut App) {
        PICK.with(|pick| *pick.borrow_mut() = Some((title, picked)));
        Self::open_inner(FinderKind::Pick, root, Arc::default(), items, Vec::new(), String::new(), None, task, window, cx)
    }

    fn open_with(
        kind: FinderKind,
        root: PathBuf,
        files: Arc<Vec<String>>,
        outline: Vec<Hit>,
        task: WeakEntity<TaskView>,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_inner(kind, root, files, outline, Vec::new(), String::new(), None, task, window, cx)
    }

    #[allow(clippy::too_many_arguments)]
    fn open_inner(
        kind: FinderKind,
        root: PathBuf,
        files: Arc<Vec<String>>,
        outline: Vec<Hit>,
        sections: Vec<(String, Vec<Hit>)>,
        title: String,
        symbols: Option<SymbolSource>,
        task: WeakEntity<TaskView>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let command = cx.new(|cx| CommandState::new(window, cx));
        let finder = cx.new(|_| Finder {
            kind,
            outline: Arc::new(outline),
            sections: Arc::new(sections),
            shown: Rc::default(),
            root,
            hits: Rc::new(Vec::new()),
            files,
            task,
            _search: Task::ready(()),
            last_query: String::new(),
            regex: false,
            mask: None,
            symbols,
            picked: None,
            searching: false,
            told: SharedString::default(),
        });
        if kind == FinderKind::Pick {
            let picked = PICK.with(|pick| pick.borrow_mut().take());
            finder.update(cx, |f, _| f.picked = picked);
        }
        if kind == FinderKind::Grep {
            use gpui_kit::component::input::{InputEvent, InputState};
            let mask = cx.new(|cx| InputState::new(window, cx).placeholder("*.rs, src/**, !*_test.go"));
            // Weak: the input is the finder's own, and would keep it alive.
            let owner = finder.downgrade();
            cx.subscribe(&mask, move |_, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let _ = owner.update(cx, |f, cx| {
                        let query = f.last_query.clone();
                        f.query(&query, cx)
                    });
                }
            })
            .detach();
            finder.update(cx, |f, _| f.mask = Some(mask));
        }
        finder.update(cx, |f, cx| f.query("", cx));

        let focused = Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let finder = finder.clone();
            let command = command.clone();
            let focused = focused.clone();
            let title = title.clone();
            dialog.close_button(false).p_0().w(px(720.)).content(move |content, window, cx| {
                if !focused.replace(true) {
                    let command = command.clone();
                    window.defer(cx, move |window, cx| command.read(cx).focus_handle(cx).focus(window, cx));
                }
                let (kind, hits, shown, pick_title) = {
                    let f = finder.read(cx);
                    (f.kind, f.hits.clone(), f.shown.clone(), f.picked.as_ref().map(|(title, _)| title.clone()).unwrap_or_default())
                };
                let mut palette = Command::new(&command)
                    .bordered(false)
                    .filterable(false)
                    .placeholder(match kind {
                        FinderKind::Files => "Go to file…".to_string(),
                        FinderKind::Grep => "Find in files…".to_string(),
                        FinderKind::Outline => "Go to declaration…".to_string(),
                        FinderKind::Places => title.clone(),
                        FinderKind::Symbols => "Go to class, function, type…".to_string(),
                        FinderKind::Pick => pick_title,
                    })
                    // The list's height follows what it holds, up to this.
                    .max_h(px(420.));
                if kind == FinderKind::Grep {
                    // A group a file: its path once, its lines under it.
                    let mut rest = &hits[..];
                    for &count in shown.iter() {
                        let (these, after) = rest.split_at(count.min(rest.len()));
                        rest = after;
                        let Some(first) = these.first() else { continue };
                        palette = palette.group(CommandGroup::new().label(first.path.clone()).items(these.iter().map(|hit| item(hit, kind))));
                    }
                } else if kind == FinderKind::Places {
                    let sections = finder.read(cx).sections.clone();
                    let mut rest = &hits[..];
                    for ((heading, all), &count) in sections.iter().zip(shown.iter()) {
                        let (these, after) = rest.split_at(count.min(rest.len()));
                        rest = after;
                        // Every section stays, even when the query empties it:
                        // the confirmed index names the section by position.
                        palette = palette.group(
                            CommandGroup::new()
                                .label(format!("{heading} ({})", all.len()))
                                .items(these.iter().map(|hit| item(hit, kind))),
                        );
                    }
                } else {
                    palette = palette.items(hits.iter().map(|hit| item(hit, kind)));
                }
                let query_owner = finder.clone();
                let confirm_owner = finder.clone();
                if kind == FinderKind::Grep {
                    let options = finder.clone();
                    palette = palette.footer(move |_, _, cx| {
                        let f = options.read(cx);
                        let (regex, mask) = (f.regex, f.mask.clone());
                        let told = if f.searching { SharedString::from("Searching…") } else { f.told.clone() };
                        let toggle = options.clone();
                        h_flex()
                            .gap_2()
                            .px_3()
                            .py_1p5()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                div()
                                    .id("grep-mask-hint")
                                    .child("Files")
                                    .tooltip(|window, cx| {
                                        gpui_kit::component::tooltip::Tooltip::new(
                                            "Globs, as rg -g: *.rs for Rust files, src/** for a folder, ! before one leaves those out. Commas between them.",
                                        )
                                        .build(window, cx)
                                    }),
                            )
                            .children(mask.map(|m| div().w(px(240.)).child(gpui_kit::component::input::Input::new(&m).small())))
                            .child(div().flex_1().min_w_0().truncate().text_right().child(told))
                            .child(
                                gpui_kit::component::button::Button::new("grep-regex")
                                    .xsmall()
                                    .ghost()
                                    .label(".* Regex")
                                    .selected(regex)
                                    .tooltip("The query as a regular expression")
                                    .on_click(move |_, _, cx| {
                                        toggle.update(cx, |f, cx| {
                                            f.regex = !f.regex;
                                            let query = f.last_query.clone();
                                            f.query(&query, cx);
                                        })
                                    }),
                            )
                    });
                }
                content.child(
                    palette
                        .on_query(move |query, _, cx| {
                            let _ = query_owner.update(cx, |f, cx| f.query(query, cx));
                        })
                        .on_confirm(move |index, window, cx| {
                            let _ = confirm_owner.update(cx, |f, cx| f.confirm(index, window, cx));
                        }),
                )
            })
        });
    }

    fn query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.last_query = query.to_string();
        let query = query.trim().to_string();
        match self.kind {
            FinderKind::Places => {
                let needle = query.to_lowercase();
                let matches = |h: &Hit| {
                    needle.is_empty() || h.text.to_lowercase().contains(&needle) || h.path.to_lowercase().contains(&needle)
                };
                let mut hits = Vec::new();
                let mut shown = Vec::new();
                for (_, section) in self.sections.iter() {
                    let before = hits.len();
                    hits.extend(section.iter().filter(|h| matches(h)).cloned());
                    shown.push(hits.len() - before);
                }
                self.shown = Rc::new(shown);
                self.set_hits(hits, cx);
            }
            FinderKind::Outline | FinderKind::Pick => {
                let needle = query.to_lowercase();
                let hits = self
                    .outline
                    .iter()
                    .filter(|h| needle.is_empty() || h.text.split(DETAIL).next().unwrap_or_default().to_lowercase().contains(&needle))
                    .cloned()
                    .collect();
                self.set_hits(hits, cx);
            }
            FinderKind::Files => {
                let files = self.files.clone();
                self._search = cx.spawn(async move |this, cx| {
                    let hits = cx.background_spawn(async move { match_files(&files, &query) }).await;
                    let _ = this.update(cx, |f, cx| f.set_hits(hits, cx));
                });
            }
            FinderKind::Symbols => {
                let Some(symbols) = self.symbols.clone() else { return };
                self._search = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_millis(150)).await;
                    let hits = cx.background_spawn(async move { symbols(&query) }).await;
                    let _ = this.update(cx, |f, cx| f.set_hits(hits, cx));
                });
            }
            FinderKind::Grep => {
                if query.len() < 2 {
                    // The search for what was typed before must not land after this.
                    self._search = Task::ready(());
                    self.searching = false;
                    self.set_hits(Vec::new(), cx);
                    return;
                }
                self.searching = true;
                let root = self.root.clone();
                let regex = self.regex;
                let mask = self.mask.as_ref().map(|m| m.read(cx).value().to_string()).unwrap_or_default();
                self._search = cx.spawn(async move |this, cx| {
                    // Wait for typing to pause; a newer query replaces this task.
                    cx.background_executor().timer(Duration::from_millis(200)).await;
                    // Dropped with this task when a newer query comes: the walk ends.
                    let stop = search::StopOnDrop::default();
                    let flag = stop.flag();
                    let hits = cx.background_spawn(async move { grep(&root, &query, regex, &mask, &flag) }).await;
                    let _ = this.update(cx, |f, cx| f.set_hits(hits, cx));
                });
            }
        }
    }

    fn set_hits(&mut self, hits: Vec<Hit>, cx: &mut Context<Self>) {
        if self.kind == FinderKind::Grep {
            self.searching = false;
            // The hits come sorted by path: a run of them is a file.
            let mut shown: Vec<usize> = Vec::new();
            for (ix, hit) in hits.iter().enumerate() {
                match ix.checked_sub(1).map(|p| &hits[p]) {
                    Some(prev) if prev.path == hit.path => *shown.last_mut().unwrap() += 1,
                    _ => shown.push(1),
                }
            }
            let matches: usize = hits.iter().map(|h| h.ranges.len().max(1)).sum();
            let mut told = match (matches, shown.len()) {
                // The list says so itself.
                (0, _) => String::new(),
                (1, _) => "1 match".to_string(),
                (n, 1) => format!("{n} matches in 1 file"),
                (n, f) => format!("{n} matches in {f} files"),
            };
            if hits.len() >= GREP_LIMIT {
                told = format!("{told}, the first {GREP_LIMIT} lines");
            }
            self.told = told.into();
            self.shown = Rc::new(shown);
        }
        self.hits = Rc::new(hits);
        cx.notify();
        // The dialog content reads the hits when the window redraws.
        cx.refresh_windows();
    }

    fn confirm(&mut self, index: IndexPath, window: &mut Window, cx: &mut Context<Self>) {
        let row = match self.kind {
            FinderKind::Places | FinderKind::Grep => self.shown.iter().take(index.section).sum::<usize>() + index.row,
            _ => index.row,
        };
        let Some(hit) = self.hits.get(row).cloned() else { return };
        window.close_dialog(cx);
        if let Some((_, picked)) = &self.picked {
            return picked(hit.path, window, cx);
        }
        let path = self.root.join(&hit.path);
        let line = hit.line.saturating_sub(1);
        // Go to file is for a file to work in; the other lists are places to look at.
        let how = if self.kind == FinderKind::Files { crate::task::OpenAs::Kept } else { crate::task::OpenAs::Preview };
        let _ = self.task.update(cx, |t, cx| t.open_file_as(path, line, hit.col, how, window, cx));
    }
}

fn item(hit: &Hit, kind: FinderKind) -> CommandItem {
    let hit = hit.clone();
    let label = match kind {
        FinderKind::Files => hit.path.clone(),
        FinderKind::Outline => hit.text.split(DETAIL).next().unwrap_or_default().trim().to_string(),
        FinderKind::Symbols => format!("{} {}:{}", hit.text, hit.path, hit.line),
        FinderKind::Pick => hit.text.clone(),
        FinderKind::Grep | FinderKind::Places => format!("{}:{} {}", hit.path, hit.line, hit.text),
    };
    CommandItem::new().label(label).child(move |_, cx| {
        let (dir, name) = match hit.path.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), hit.path.clone()),
        };
        match kind {
            // A declaration as the Structure panel shows it: its kind's
            // letter, its name indented by its depth.
            FinderKind::Outline if hit.text.contains(DETAIL) => {
                let (name, what) = hit.text.split_once(DETAIL).unwrap_or((&hit.text, ""));
                let depth = (name.len() - name.trim_start().len()) / 2;
                let (mark, color) = kind_mark(what);
                h_flex()
                    .gap_3()
                    .min_w_0()
                    .flex_1()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .pl(px(14. * depth as f32))
                            .child(div().w(px(12.)).flex_none().text_xs().font_semibold().text_color(color).child(mark))
                            .child(div().min_w_0().truncate().text_sm().child(name.trim_start().to_string())),
                    )
                    .child(div().flex_none().text_xs().text_color(cx.theme().muted_foreground).child(format!(":{}", hit.line)))
                    .into_any_element()
            }
            FinderKind::Outline => h_flex()
                .gap_3()
                .min_w_0()
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_sm()
                        .child(hit.text.clone()),
                )
                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!(":{}", hit.line)))
                .into_any_element(),
            FinderKind::Symbols => {
                let (symbol, detail) = hit.text.split_once(DETAIL).unwrap_or((&hit.text, ""));
                h_flex()
                    .gap_3()
                    .min_w_0()
                    .child(div().flex_none().font_family(cx.theme().mono_font_family.clone()).text_sm().child(symbol.to_string()))
                    .child(div().flex_none().text_xs().text_color(cx.theme().muted_foreground).child(detail.to_string()))
                    .child(div().flex_1())
                    .child(div().truncate().text_xs().text_color(cx.theme().muted_foreground).child(format!("{}:{}", hit.path, hit.line)))
                    .into_any_element()
            }
            FinderKind::Pick => {
                let (name, detail) = hit.text.split_once(DETAIL).unwrap_or((&hit.text, ""));
                // A second line of detail when it has one: more than fits beside the name.
                let (detail, more) = detail.split_once('\n').unwrap_or((detail, ""));
                let muted = cx.theme().muted_foreground;
                v_flex()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_3()
                            .min_w_0()
                            .child(div().flex_none().font_family(cx.theme().mono_font_family.clone()).text_sm().child(name.to_string()))
                            .child(div().truncate().text_xs().text_color(muted).child(detail.to_string())),
                    )
                    .children((!more.is_empty()).then(|| div().truncate().text_xs().text_color(muted).child(more.to_string())))
                    .into_any_element()
            }
            FinderKind::Files => h_flex()
                .gap_2()
                .min_w_0()
                .child(crate::file_icons::icon(crate::file_icons::file(&name), cx))
                .child(div().flex_none().child(name))
                .child(div().truncate().text_xs().text_color(cx.theme().muted_foreground).child(dir))
                .into_any_element(),
            // A line each: the code, and where it is (its file heads the group in Find in files).
            FinderKind::Grep | FinderKind::Places => h_flex()
                .gap_3()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_sm()
                        .child(highlighted(&hit, cx)),
                )
                .child(
                    div()
                        .flex_shrink(1.)
                        .max_w(px(260.))
                        .truncate()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(if kind == FinderKind::Grep { format!(":{}", hit.line) } else { format!("{}:{}", hit.path, hit.line) }),
                )
                .into_any_element(),
        }
    })
}

/// A declaration's kind as a letter and a color, as IDEs mark them: the
/// Structure panel's and Go to declaration's rows.
pub fn kind_mark(kind: &str) -> (&'static str, Hsla) {
    let has = |words: &[&str]| words.iter().any(|w| kind.contains(w));
    let (mark, hue) = if has(&["method", "constructor"]) {
        ("m", 0.58)
    } else if has(&["function", "fn"]) {
        ("f", 0.58)
    } else if has(&["interface", "trait", "protocol"]) {
        ("I", 0.38)
    } else if has(&["enum"]) {
        ("E", 0.78)
    } else if has(&["impl"]) {
        ("i", 0.78)
    } else if has(&["class", "struct", "object", "record"]) {
        ("C", 0.08)
    } else if has(&["type"]) {
        ("T", 0.48)
    } else if has(&["module", "namespace", "package", "mod"]) {
        ("M", 0.12)
    } else if has(&["field", "property", "const", "static", "var", "let"]) {
        ("v", 0.15)
    } else {
        return ("·", hsla(0., 0., 0.5, 1.));
    };
    (mark, hsla(hue, 0.6, 0.5, 1.))
}

/// The line without its indent, the matches marked.
fn highlighted(hit: &Hit, cx: &App) -> StyledText {
    let indent = hit.text.len() - hit.text.trim_start().len();
    let text = hit.text.trim().to_string();
    let style = HighlightStyle {
        background_color: Some(hsla(0.14, 0.95, 0.55, 0.35)),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    let _ = cx;
    let ranges: Vec<_> = hit
        .ranges
        .iter()
        .filter_map(|r| {
            let (start, end) = (r.start.checked_sub(indent)?, r.end.saturating_sub(indent).min(text.len()));
            (start < end && text.is_char_boundary(start) && text.is_char_boundary(end)).then(|| (start..end, style))
        })
        .collect();
    StyledText::new(text).with_highlights(ranges)
}

/// The declarations whose name has `query` in it, whatever the case: those
/// it begins first, short names before long ones.
pub fn match_symbols(all: &[Hit], query: &str, limit: usize) -> Vec<Hit> {
    let needle = query.to_lowercase();
    let mut found: Vec<(bool, usize, &Hit)> = all
        .iter()
        .filter_map(|hit| {
            let name = hit.text.split(DETAIL).next().unwrap_or_default().to_lowercase();
            name.contains(&needle).then(|| (!name.starts_with(&needle), name.len(), hit))
        })
        .collect();
    found.sort_by_key(|(inside, len, _)| (*inside, *len));
    found.into_iter().take(limit).map(|(_, _, hit)| hit.clone()).collect()
}

/// Subsequence match, scored so that consecutive characters, matches in the
/// file name and short paths rank first.
fn match_files(files: &[String], query: &str) -> Vec<Hit> {
    let query: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    let mut scored: Vec<(i64, &String)> = files
        .iter()
        .filter_map(|path| score(path, &query).map(|s| (s, path)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
    scored
        .into_iter()
        .take(FILE_LIMIT)
        .map(|(_, path)| Hit { path: path.clone(), line: 0, col: 0, text: String::new(), ranges: Vec::new() })
        .collect()
}

pub(crate) fn score(path: &str, query: &[char]) -> Option<i64> {
    if query.is_empty() {
        return Some(-(path.len() as i64));
    }
    let lower: Vec<char> = path.to_lowercase().chars().collect();
    let name_start = lower.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);
    let mut score = 0i64;
    let mut qi = 0;
    let mut prev: Option<usize> = None;
    for (i, &c) in lower.iter().enumerate() {
        if qi < query.len() && c == query[qi] {
            score += 1;
            if prev == Some(i.wrapping_sub(1)) {
                score += 5;
            }
            if i >= name_start {
                score += 3;
            }
            if i == name_start || lower.get(i.wrapping_sub(1)).is_some_and(|p| matches!(p, '/' | '_' | '-' | '.')) {
                score += 4;
            }
            prev = Some(i);
            qi += 1;
        }
    }
    (qi == query.len()).then_some(score * 100 - path.len() as i64)
}

fn grep(root: &PathBuf, query: &str, regex: bool, mask: &str, stop: &std::sync::atomic::AtomicBool) -> Vec<Hit> {
    let matcher = if regex { search::regex(query) } else { search::literal(query) };
    let Some(matcher) = matcher else { return Vec::new() };
    search::search_in(root, &matcher, search::mask(root, mask), 20, 300, GREP_LIMIT, stop)
        .into_iter()
        .map(|m| {
            // The cursor goes to the first match.
            let col = m.matches.first().map_or(0, |r| m.text[..r.start].chars().count() as u32);
            Hit { path: m.path.to_string_lossy().into_owned(), line: m.line as u32, col, text: m.text, ranges: m.matches }
        })
        .collect()
}
