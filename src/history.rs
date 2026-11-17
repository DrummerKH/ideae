use std::{path::PathBuf, rc::Rc};

use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState, MoveDown, MoveUp},
    popover::Popover,
    diff::{Diff, DiffFile, DiffMode, DiffState},
    h_flex,
    list::ListItem,
    scroll::Scrollbar,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use crate::{git, graph, task::TaskView};

/// The width of a lane of the graph, and how often a visible graph asks
/// whether a branch moved.
const LANE: Pixels = px(12.);
/// The graph's column at its widest: lanes past it are drawn narrower, down
/// to `NARROW_LANE`, rather than taking the messages' room.
const GRAPH_WIDTH: Pixels = px(150.);
const NARROW_LANE: Pixels = px(5.);
/// A row of the graph: its lanes go from its top to its bottom, so that
/// rows join up.
const GRAPH_ROW: Pixels = px(24.);
const REFS_CHECK: std::time::Duration = std::time::Duration::from_secs(20);

/// The graph of every branch, as History shows it when asked to: the rows
/// laid out for the commits listed, and what tells it is to be read again.
#[derive(Default)]
struct Graph {
    rows: Rc<Vec<graph::Row>>,
    lanes: usize,
    filter: git::GraphFilter,
    /// The branches as they were when read; checked while the graph is seen.
    stamp: u64,
    checked: Option<std::time::Instant>,
    /// Whether each branch has commits of its own, by branch and commit.
    known: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<(String, String), bool>>>,
    hide: Option<Entity<InputState>>,
    fetched: bool,
    /// Prefixes the remote has many branches under (`tenant/*`), not hidden:
    /// offered to hide.
    families: Vec<(String, usize)>,
    /// The commits the typed words find, by their index in the list: found
    /// among those read, as git's own search would break the lanes.
    found: Option<Rc<Vec<usize>>>,
    /// Which of them was gone to last.
    at: usize,
}

/// Branches under one prefix that make it worth offering to hide them.
const FAMILY: usize = 10;

/// Commits on the task's branch, newest first, and the selected one's diff.
pub struct HistoryView {
    root: PathBuf,
    commits: Rc<Vec<git::Commit>>,
    selected: Option<usize>,
    /// HEAD when the list was loaded; a new commit reloads it.
    head: String,
    diff: Entity<DiffState>,
    /// First changed line per file of the commit shown, for "open".
    first_lines: Rc<std::collections::HashMap<String, u32>>,
    scroll: UniformListScrollHandle,
    task: WeakEntity<TaskView>,
    /// HEAD moved since the commits were read.
    stale: bool,
    /// How many commits are asked for.
    limit: usize,
    /// The branch whose history is shown when not the task's own, and
    /// what narrows it, as typed.
    rev: String,
    typed: String,
    filter: Option<Entity<InputState>>,
    /// Whether the field's hint is the graph's, which searches rather than narrows.
    hint_graph: bool,
    _typing: Task<()>,
    _load: Task<()>,
    _show: Task<()>,
    /// A commit shown from elsewhere that the list does not reach: put back
    /// when the list is read again while it is the one looked at.
    brought: Option<git::Commit>,
    /// On: every branch, with its lanes.
    graph: Option<Graph>,
    _graph: Task<()>,
    /// The message of the commit shown past its first line, by its hash,
    /// and whether all of it is shown.
    body: Option<(String, Rc<Vec<SharedString>>)>,
    body_open: bool,
    /// The branches whose history holds the commit shown, read after it.
    containing: Option<(String, Rc<Vec<String>>)>,
    containing_open: bool,
    _containing: Task<()>,
}

/// What the field over the list takes, outside the graph.
const HINT: &str = "words  @author  file:path";

/// Lines of a long message shown before "Show all".
const BODY_LINES: usize = 4;

/// The whole history: `git log` of 6k commits takes 30 ms and the list is
/// virtualized. The cap only guards against giant monorepos.
/// The commits read at first, and four times as many with each "older":
/// a long history is megabytes a task, and seldom read to its end.
const FIRST: usize = 2_000;

impl HistoryView {
    pub fn new(root: PathBuf, task: WeakEntity<TaskView>, cx: &mut Context<Self>) -> Self {
        let diff = cx.new(|cx| DiffState::new([], cx).with_mode(DiffMode::Unified));
        cx.observe(&diff, |_, _, cx| cx.notify()).detach();
        Self {
            root,
            commits: Rc::default(),
            selected: None,
            head: String::new(),
            diff,
            first_lines: Rc::default(),
            scroll: UniformListScrollHandle::new(),
            task,
            stale: false,
            limit: FIRST,
            rev: String::new(),
            typed: String::new(),
            filter: None,
            hint_graph: false,
            _typing: Task::ready(()),
            _load: Task::ready(()),
            _show: Task::ready(()),
            brought: None,
            graph: None,
            _graph: Task::ready(()),
            body: None,
            body_open: false,
            containing: None,
            containing_open: false,
            _containing: Task::ready(()),
        }
    }

    /// Reloads the list when HEAD moved since the last load.
    pub fn sync_head(&mut self, head: &str, cx: &mut Context<Self>) {
        if head == self.head {
            return;
        }
        self.head = head.to_string();
        // Read when the tab is drawn: every task has a history, and few are looked at.
        self.stale = true;
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.stale = false;
        if let Some(graph) = &self.graph {
            let (root, limit, filter, known) = (self.root.clone(), self.limit, graph.filter.clone(), graph.known.clone());
            self._load = cx.spawn(async move |this, cx| {
                let (commits, rows, stamp, families) = cx
                    .background_spawn(async move {
                        let stamp = git::refs_stamp(&root);
                        let families: Vec<(String, usize)> = git::branch_families(&root, FAMILY).into_iter().filter(|(prefix, _)| !filter.hide.contains(prefix)).collect();
                        let read = git::graph(&root, limit, &filter, &mut known.lock().unwrap());
                        let rows = graph::layout(read.iter().map(|c| (c.commit.hash.as_str(), c.parents.as_slice())));
                        (read.into_iter().map(|c| c.commit).collect::<Vec<_>>(), rows, stamp, families)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let Some(graph) = this.graph.as_mut() else { return };
                    graph.families = families;
                    graph.lanes = rows.iter().map(graph::width).max().unwrap_or(1);
                    graph.rows = Rc::new(rows);
                    graph.stamp = stamp;
                    graph.checked = Some(std::time::Instant::now());
                    this.show_commits(commits, cx);
                });
            });
            return;
        }
        let (root, limit, query) = (self.root.clone(), self.limit, git::HistoryQuery::typed(&self.rev, &self.typed));
        self._load = cx.spawn(async move |this, cx| {
            let commits = cx.background_spawn(async move { git::history(&root, limit, &query) }).await;
            let _ = this.update(cx, |this, cx| this.show_commits(commits, cx));
        });
    }

    /// The graph of every branch, on or off. On, the branches are fetched
    /// once, and the graph read again if that brought any.
    fn toggle_graph(&mut self, cx: &mut Context<Self>) {
        if self.graph.take().is_some() {
            self.stale = true;
            cx.notify();
            return;
        }
        let root = self.root.clone();
        self._graph = cx.spawn(async move |this, cx| {
            let filter = cx
                .background_spawn({
                    let root = root.clone();
                    async move { git::graph_filter(&root) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.graph = Some(Graph { filter, ..Default::default() });
                this.stale = true;
                cx.notify();
            });
            let fetched = cx.background_spawn(async move { git::fetch_all(&root).is_ok() }).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(graph) = this.graph.as_mut() {
                    graph.fetched = fetched;
                    // What the fetch brought shows: the branches are looked at again.
                    graph.checked = None;
                    cx.notify();
                }
            });
        });
    }

    /// While the graph is seen, now and then: whether a branch moved, which
    /// reads it again. One `git for-each-ref`, off the UI thread.
    fn check_refs(&mut self, cx: &mut Context<Self>) {
        let Some(graph) = self.graph.as_mut() else { return };
        if graph.checked.is_some_and(|at| at.elapsed() < REFS_CHECK) {
            return;
        }
        graph.checked = Some(std::time::Instant::now());
        let (root, stamp) = (self.root.clone(), graph.stamp);
        self._graph = cx.spawn(async move |this, cx| {
            let now = cx.background_spawn(async move { git::refs_stamp(&root) }).await;
            if now != stamp {
                let _ = this.update(cx, |this, cx| {
                    this.stale = true;
                    cx.notify();
                });
            }
        });
    }

    fn set_graph_filter(&mut self, filter: git::GraphFilter, cx: &mut Context<Self>) {
        let Some(graph) = self.graph.as_mut() else { return };
        if graph.filter == filter {
            return;
        }
        graph.filter = filter.clone();
        self.stale = true;
        let root = self.root.clone();
        cx.background_spawn(async move { git::set_graph_filter(&root, &filter) }).detach();
        cx.notify();
    }

    /// The other end of a long line: selected, and scrolled to.
    fn jump_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.select(ix, cx);
        self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        cx.notify();
    }

    /// One click for a family of branches the remote has many of: its
    /// pattern joins the hidden ones, in the field too.
    fn hide_family(&mut self, prefix: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(graph) = self.graph.as_mut() else { return };
        let mut filter = graph.filter.clone();
        if !filter.hide.contains(&prefix) {
            filter.hide.push(prefix);
        }
        graph.families.retain(|(p, _)| !filter.hide.contains(p));
        if let Some(input) = graph.hide.clone() {
            let shown = filter.hide.join(", ");
            input.update(cx, |input, cx| input.set_value(shown, window, cx));
        }
        self.set_graph_filter(filter, cx);
    }

    fn hide_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<InputState>> {
        let graph = self.graph.as_mut()?;
        if let Some(input) = &graph.hide {
            return Some(input.clone());
        }
        let shown = graph.filter.hide.join(", ");
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("tenant/*, dependabot/*").default_value(shown));
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let hide: Vec<String> = input.read(cx).value().split([',', ' ']).map(str::trim).filter(|g| !g.is_empty()).map(str::to_string).collect();
            this._typing = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                let _ = this.update(cx, |this, cx| {
                    let Some(own_only) = this.graph.as_ref().map(|g| g.filter.own_only) else { return };
                    this.set_graph_filter(git::GraphFilter { hide, own_only }, cx);
                });
            });
        })
        .detach();
        if let Some(graph) = self.graph.as_mut() {
            graph.hide = Some(input.clone());
        }
        Some(input)
    }

    fn show_commits(&mut self, mut commits: Vec<git::Commit>, cx: &mut Context<Self>) {
        // Keep the selection on the same commit when it is still listed.
        let selected_hash = self.selected.and_then(|ix| self.commits.get(ix)).map(|c| c.hash.clone());
        // One brought from beyond the list is still the one looked at: it stays first.
        if let Some(brought) = self.brought.clone().filter(|b| selected_hash.as_ref() == Some(&b.hash)) {
            if !commits.iter().any(|c| c.hash == brought.hash) {
                commits.insert(0, brought);
            }
        }
        self.commits = Rc::new(commits);
        let ix = selected_hash
            .and_then(|h| self.commits.iter().position(|c| c.hash == h))
            .or((!self.commits.is_empty()).then_some(0));
        self.selected = None;
        if let Some(ix) = ix {
            self.select(ix, cx);
        }
        if self.graph.is_some() {
            self.find_in_graph(cx);
        }
        cx.notify();
    }

    /// Shows a commit from elsewhere (blame): from the list when loaded,
    /// else fetched and put first.
    pub fn select_hash(&mut self, hash: &str, cx: &mut Context<Self>) {
        // Short or full: blame gives full hashes, the branch bar a short one.
        if let Some(ix) = self.commits.iter().position(|c| c.hash.starts_with(hash)) {
            self.select(ix, cx);
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            return;
        }
        // Older than the list goes: asked of git, off the UI thread.
        let (root, hash) = (self.root.clone(), hash.to_string());
        self._show = cx.spawn(async move |this, cx| {
            let Some(commit) = cx.background_spawn(async move { git::commit_info(&root, &hash) }).await else { return };
            let _ = this.update(cx, |this, cx| {
                let mut commits = (*this.commits).clone();
                this.brought = Some(commit.clone());
                commits.insert(0, commit);
                this.commits = Rc::new(commits);
                this.selected = None;
                this.select(0, cx);
                this.scroll.scroll_to_item(0, ScrollStrategy::Top);
            });
        });
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.selected == Some(ix) {
            return;
        }
        let Some(commit) = self.commits.get(ix) else { return };
        self.selected = Some(ix);
        let (root, hash) = (self.root.clone(), commit.hash.clone());
        self._show = cx.spawn(async move |this, cx| {
            let (files, lines) = cx
                .background_spawn({
                    let hash = hash.clone();
                    async move {
                        let patch = git::commit_patch(&root, &hash);
                        let body: Vec<SharedString> = git::commit_body(&root, &hash).lines().map(|l| SharedString::from(l.to_string())).collect();
                        (crate::task::with_languages(DiffFile::parse(&patch).unwrap_or_default()), (git::first_changed_lines(&patch), body))
                    }
                })
                .await;
            let (lines, body) = lines;
            let lines = Rc::new(lines);
            let _ = this.update(cx, |this, cx| {
                this.first_lines = lines;
                this.body = Some((hash, Rc::new(body)));
                this.body_open = false;
                this.diff.update(cx, |d, cx| d.set_files(files, cx));
            });
        });
        // Which branches hold it: after the diff, for that one commit alone.
        let (root, hash) = (self.root.clone(), self.commits[ix].hash.clone());
        self.containing_open = false;
        self._containing = cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn({
                    let hash = hash.clone();
                    async move { git::branches_containing(&root, &hash) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.containing = Some((hash, Rc::new(found)));
                cx.notify();
            });
        });
        cx.notify();
    }

    /// Another branch's history, or back to the task's own.
    fn pick_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const OWN: &str = "\u{1}own";
        let (root, task, this) = (self.root.clone(), self.task.clone(), cx.weak_entity());
        cx.spawn_in(window, async move |_, cx| {
            let from = root.clone();
            let rows = cx.background_spawn(async move { git::branch_rows(&from) }).await;
            let detail = crate::finder::DETAIL;
            let mut items = vec![crate::finder::Hit { text: format!("This task's branch{detail}what is checked out here"), path: OWN.into(), line: 0, col: 0, ranges: Vec::new() }];
            items.extend(rows.into_iter().map(|row| crate::finder::Hit {
                text: format!("{}{detail}{} · {} · {}", row.name, if row.remote { "remote" } else { "local" }, row.when, row.subject),
                path: row.name,
                line: 0,
                col: 0,
                ranges: Vec::new(),
            }));
            let _ = cx.update(|window, cx| {
                crate::finder::Finder::open_pick(
                    "History of…".into(),
                    root,
                    items,
                    Rc::new(move |name, _, cx| {
                        let _ = this.update(cx, |history, cx| {
                            history.rev = if name == OWN { String::new() } else { name };
                            history.stale = true;
                            cx.notify();
                        });
                    }),
                    task,
                    window,
                    cx,
                )
            });
        })
        .detach();
    }

    /// The field that narrows the list, made when first drawn: typing
    /// reads the history again once it pauses.
    fn filter_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.filter {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(HINT));
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { shift, .. } = event {
                this.step_found(if *shift { -1 } else { 1 }, cx);
                return;
            }
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let typed = input.read(cx).value().trim().to_string();
            if typed == this.typed {
                return;
            }
            this._typing = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                let _ = this.update(cx, |this, cx| {
                    this.typed = typed;
                    // The graph's lanes are laid out for every commit: it is searched, not narrowed.
                    if this.graph.is_some() {
                        this.find_in_graph(cx);
                        if let Some(&first) = this.graph.as_ref().and_then(|g| g.found.as_ref()).and_then(|f| f.first()) {
                            this.jump_to(first, cx);
                        }
                    } else {
                        this.stale = true;
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        self.filter = Some(input.clone());
        input
    }

    /// The commits of the graph the typed words and `@author` find, in its
    /// subject, branches and tags, or as the start of its hash.
    fn find_in_graph(&mut self, cx: &mut Context<Self>) {
        let Some(graph) = self.graph.as_mut() else { return };
        let query = git::HistoryQuery::typed("", &self.typed);
        if query.text.is_empty() && query.author.is_empty() {
            graph.found = None;
            cx.notify();
            return;
        }
        let words: Vec<String> = query.text.to_lowercase().split_whitespace().map(str::to_string).collect();
        let author = query.author.to_lowercase();
        let found: Vec<usize> = self
            .commits
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                if !author.is_empty() && !c.author.to_lowercase().contains(&author) {
                    return false;
                }
                let (subject, refs) = (c.subject.to_lowercase(), c.refs.to_lowercase());
                words.iter().all(|w| subject.contains(w.as_str()) || refs.contains(w.as_str()) || c.hash.starts_with(w.as_str()))
            })
            .map(|(ix, _)| ix)
            .collect();
        graph.at = self.selected.and_then(|s| found.iter().position(|&ix| ix == s)).unwrap_or(0);
        graph.found = Some(Rc::new(found));
        cx.notify();
    }

    /// The next or previous commit found in the graph, round the ends.
    fn step_found(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(graph) = self.graph.as_mut() else { return };
        let Some(found) = graph.found.clone().filter(|f| !f.is_empty()) else { return };
        let len = found.len() as isize;
        graph.at = (graph.at as isize + by).rem_euclid(len) as usize;
        let to = found[graph.at];
        self.jump_to(to, cx);
    }

    fn render_list(&self, filter: Entity<InputState>, hide: Option<Entity<InputState>>, cx: &mut Context<Self>) -> impl IntoElement {
        let commits = self.commits.clone();
        let selected = self.selected;
        let owner = cx.weak_entity();
        let graph = self.graph.as_ref().map(|g| (g.rows.clone(), g.lanes, g.filter.own_only));
        let found = self.graph.as_ref().and_then(|g| g.found.clone());
        let palette = palette(cx);
        v_flex()
            .map(|list| match graph {
                // The graph is a table: it takes most of the width.
                Some(_) => list.w(relative(0.68)).min_w(px(480.)),
                None => list.w(relative(0.35)).min_w(px(200.)).max_w(px(340.)),
            })
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_1()
                            .pb_1()
                            .child(
                                Button::new("history-graph")
                                    .xsmall()
                                    .ghost()
                                    .label("⑂ Graph")
                                    .selected(graph.is_some())
                                    .tooltip("Every branch, with lanes for where each left and joined")
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_graph(cx))),
                            )
                            .map(|row| match (&graph, hide) {
                                (Some((_, _, own_only)), Some(hide)) => {
                                    let found = self.graph.as_ref().and_then(|g| g.found.clone().map(|f| (g.at, f)));
                                    row.child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            // Up and down in the field go from one commit found to the next.
                                            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                                                cx.stop_propagation();
                                                this.step_found(-1, cx);
                                            }))
                                            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                                                cx.stop_propagation();
                                                this.step_found(1, cx);
                                            }))
                                            .child(Input::new(&filter).xsmall()),
                                    )
                                    .when_some(found, |row, (at, found)| {
                                        let told = match found.len() {
                                            0 => "no matches".to_string(),
                                            n if found.get(at).is_some_and(|&ix| selected == Some(ix)) => format!("{} of {n}", at + 1),
                                            1 => "1 match".to_string(),
                                            n => format!("{n} matches"),
                                        };
                                        row.child(div().flex_none().px_1().child(told)).when(found.len() > 1, |row| {
                                            row.child(Button::new("found-previous").xsmall().ghost().label("↑").tooltip("The previous one found (Shift+Enter, Up)").on_click(cx.listener(|this, _, _, cx| this.step_found(-1, cx))))
                                                .child(Button::new("found-next").xsmall().ghost().label("↓").tooltip("The next one found (Enter, Down)").on_click(cx.listener(|this, _, _, cx| this.step_found(1, cx))))
                                        })
                                    })
                                    .child(self.graph_options(hide, *own_only, cx))
                                }
                                _ => row
                                    .child(
                                        Button::new("history-branch")
                                            .xsmall()
                                            .ghost()
                                            .label(if self.rev.is_empty() { "⎇ this branch ▾".to_string() } else { format!("⎇ {} ▾", self.rev) })
                                            .tooltip("The history of another branch")
                                            .on_click(cx.listener(|this, _, window, cx| this.pick_branch(window, cx))),
                                    )
                                    .child(div().flex_1().min_w_0().child(Input::new(&filter).xsmall())),
                            }),
                    )
                    .children(self.graph.as_ref().and_then(|g| g.families.first().cloned()).map(|(prefix, count)| {
                        h_flex()
                            .gap_2()
                            .pb_1()
                            .child(format!("{count} branches under {}", prefix.trim_end_matches('*')))
                            .child(Button::new("hide-family").xsmall().ghost().label("Hide").tooltip("Leave them out of the graph; the pattern goes into Filter ▾").on_click(cx.listener(
                                move |this, _, window, cx| this.hide_family(prefix.clone(), window, cx),
                            )))
                    }))
                    .child(
                        h_flex()
                            .justify_between()
                            .child(match self.commits.len() { 1 => "1 commit".to_string(), n => format!("{n} commits") })
                            .when(self.commits.len() >= self.limit, |row| {
                                row.child(Button::new("older-commits").xsmall().ghost().label("older…").tooltip("Reads further back").on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.limit *= 4;
                                        this.stale = true;
                                        cx.notify();
                                    },
                                )))
                            }),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list("commits", commits.len(), move |range, _, cx| {
                            range
                                .map(|ix| {
                                    let commit = &commits[ix];
                                    let owner = owner.clone();
                                    if let Some((rows, lanes, _)) = &graph {
                                        // A commit brought from beyond the list has no row of the graph.
                                        let offset = commits.len() - rows.len();
                                        let row = ix.checked_sub(offset).map(|i| (rows.clone(), i));
                                        // Where its long line leads: told, and a click away.
                                        let jump = row.as_ref().and_then(|(rows, i)| {
                                            let row = &rows[*i];
                                            let (to, way) = match row.down_to.iter().flatten().next() {
                                                Some(&to) => (to, "down"),
                                                None => (*row.up_from.first()?, "up"),
                                            };
                                            let target = commits.get(to + offset)?;
                                            let rows_away = to.abs_diff(*i);
                                            Some((to + offset, SharedString::from(format!("Goes on {rows_away} rows {way}, to {} {}. Click to go there.", target.short, target.subject))))
                                        });
                                        let dim = found.as_ref().is_some_and(|f| f.binary_search(&ix).is_err());
                                        return graph_row(ix, commit, row, *lanes, palette, selected == Some(ix), dim, jump, owner, cx);
                                    }
                                    ListItem::new(ix)
                                        .selected(selected == Some(ix))
                                        .h_12()
                                        .px_3()
                                        .w_full()
                                        .child(
                                            v_flex()
                                                .gap_0p5()
                                                .min_w_0()
                                                .flex_1()
                                                .child(
                                                    h_flex()
                                                        .gap_1p5()
                                                        .min_w_0()
                                                        // A merge has seldom anything to read: dimmer.
                                                        .child(div().flex_1().text_sm().min_w_0().truncate().when(commit.merge, |s| s.text_color(cx.theme().muted_foreground)).child(commit.subject.clone()))
                                                        // The branches and tags at this commit, after its subject: one, and how many more.
                                                        .children(ref_badges(ix, commit, cx)),
                                                )
                                                .child(
                                                    h_flex()
                                                        .gap_2()
                                                        .text_xs()
                                                        .text_color(cx.theme().muted_foreground)
                                                        .child(
                                                            div()
                                                                .font_family(cx.theme().mono_font_family.clone())
                                                                .child(commit.short.clone()),
                                                        )
                                                        .child(div().flex_1().min_w_0().truncate().child(commit.author.clone()))
                                                        .child(commit.date.clone()),
                                                ),
                                        )
                                        .on_click(move |_, _, cx| {
                                            let _ = owner.update(cx, |this, cx| this.select(ix, cx));
                                        })
                                        .into_any_element()
                                })
                                .collect()
                        })
                        .size_full()
                        .track_scroll(&self.scroll),
                    )
                    .child(Scrollbar::vertical(&self.scroll)),
            )
    }
}

impl HistoryView {
    /// "Filter ▾" beside the graph's search: which branches it leaves out.
    fn graph_options(&self, hide: Entity<InputState>, own_only: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden = self.graph.as_ref().map_or(0, |g| g.filter.hide.len());
        let owner = cx.weak_entity();
        let label = match (hidden, own_only) {
            (0, false) => "Filter ▾".to_string(),
            (n, own) => format!("Filter · {} ▾", n + own as usize),
        };
        Popover::new("graph-options")
            .anchor(Anchor::TopRight)
            .trigger(Button::new("graph-options-button").xsmall().ghost().label(label).selected(hidden > 0 || own_only).tooltip("Which branches the graph leaves out"))
            .content(move |_, _, cx| {
                let own_only = owner.upgrade().and_then(|h| h.read(cx).graph.as_ref().map(|g| g.filter.own_only)).unwrap_or(false);
                let owner = owner.clone();
                v_flex()
                    .w(px(340.))
                    .gap_2()
                    .text_xs()
                    .child(div().text_color(cx.theme().muted_foreground).child("Hide branches, by pattern"))
                    .child(Input::new(&hide).xsmall())
                    .child(
                        Checkbox::new("graph-own")
                            .label("Only branches with commits of their own")
                            .checked(own_only)
                            .tooltip("Leaves out branches that only merge others in (a bot merging production into them)")
                            .on_click(move |&on, _, cx| {
                                let _ = owner.update(cx, |this, cx| {
                                    let Some(hide) = this.graph.as_ref().map(|g| g.filter.hide.clone()) else { return };
                                    this.set_graph_filter(git::GraphFilter { hide, own_only: on }, cx);
                                });
                            }),
                    )
            })
    }

    /// The message past its subject: the first lines, and all of them on
    /// asking, scrolled within a part of the height.
    /// "In main, origin/main", or the first few of many and the rest on a click.
    fn render_containing(&self, hash: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        const FEW: usize = 4;
        let (_, branches) = self.containing.as_ref().filter(|(h, _)| h == hash)?;
        if branches.is_empty() {
            return None;
        }
        let open = self.containing_open || branches.len() <= FEW;
        let shown = if open { branches.join(", ") } else { format!("{}, …", branches[..FEW].join(", ")) };
        let told = match branches.len() {
            1 => format!("In {shown}"),
            n => format!("In {n} branches: {shown}"),
        };
        Some(
            div()
                .id("commit-branches")
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .when(branches.len() > FEW, |row| {
                    row.cursor_pointer().hover(|s| s.text_color(cx.theme().foreground)).on_click(cx.listener(|this, _, _, cx| {
                        this.containing_open = !this.containing_open;
                        cx.notify();
                    }))
                })
                .child(if branches.len() > FEW && !open { format!("{told} ▾") } else { told })
                .into_any_element(),
        )
    }

    fn render_body(&self, hash: &str, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, body) = self.body.as_ref().filter(|(h, _)| h == hash)?;
        if body.iter().all(|l| l.trim().is_empty()) {
            return None;
        }
        let long = body.len() > BODY_LINES;
        let open = self.body_open;
        let shown = if long && !open { &body[..BODY_LINES] } else { &body[..] };
        let muted = cx.theme().muted_foreground;
        Some(
            v_flex()
                .pt_1()
                .child(
                    div()
                        .id("commit-body")
                        .when(open, |d| d.max_h(window.viewport_size().height * 0.4).overflow_y_scroll())
                        .text_sm()
                        .text_color(cx.theme().foreground.opacity(0.85))
                        .children(shown.iter().map(|line| if line.trim().is_empty() { div().h_2().into_any_element() } else { div().child(line.clone()).into_any_element() })),
                )
                .when(long, |this| {
                    this.child(
                        div()
                            .id("commit-body-more")
                            .text_xs()
                            .text_color(muted)
                            .cursor_pointer()
                            .hover(|s| s.text_color(cx.theme().foreground))
                            .child(if open { "Show less".to_string() } else { format!("Show all · {} lines", body.len()) })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.body_open = !this.body_open;
                                cx.notify();
                            })),
                    )
                })
                .into_any_element(),
        )
    }
}

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.stale {
            self.load(cx);
        }
        let filter = self.filter_input(window, cx);
        if self.hint_graph != self.graph.is_some() {
            self.hint_graph = self.graph.is_some();
            let hint = if self.hint_graph { "find: words  @author  hash" } else { HINT };
            filter.update(cx, |input, cx| input.set_placeholder(hint, window, cx));
        }
        let hide = self.hide_input(window, cx);
        if self.graph.is_some() {
            self.check_refs(cx);
        }
        let commit = self.selected.and_then(|ix| self.commits.get(ix)).cloned();
        let root = self.root.clone();
        let task = self.task.clone();
        h_flex()
            .size_full()
            .child(self.render_list(filter, hide, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .when_some(commit, |this, c| {
                        this.child(
                            v_flex()
                                .px_3()
                                .py_2()
                                .gap_0p5()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(div().font_semibold().child(c.subject.clone()))
                                .children(self.render_body(&c.hash, window, cx))
                                .children(self.render_containing(&c.hash, cx))
                                .child(div().min_w_0().truncate().text_xs().text_color(cx.theme().muted_foreground).child(format!("{} · {} · {}", c.short, c.author, c.date)))
                                // A narrow pane puts the buttons on more lines rather than cut their labels.
                                .child(
                                    h_flex()
                                        .flex_wrap()
                                        .gap_x_1()
                                        .ml(px(-6.))
                                        .children([("commit-pick", "Cherry-pick", "Take this commit into the task's branch", 0), ("commit-revert", "Revert", "Commit the opposite of this commit", 1), ("commit-task", "New task from here", "A task in a worktree of its own, starting at this commit", 2)].map(
                                            |(id, label, tip, what)| {
                                                let (task, hash) = (self.task.clone(), c.hash.clone());
                                                Button::new(id).xsmall().ghost().label(label).tooltip(tip).on_click(move |_, window, cx| {
                                                    let hash = hash.clone();
                                                    let _ = task.update(cx, |t, cx| t.commit_action(what, hash, window, cx));
                                                })
                                            },
                                        ))
                                        .child({
                                            let (task, hash) = (self.task.clone(), c.hash.clone());
                                            Button::new("commit-compare")
                                                .xsmall()
                                                .ghost()
                                                .label("Compare with working tree")
                                                .tooltip("What is here now that this commit did not have")
                                                .on_click(move |_, window, cx| {
                                                    let hash = hash.clone();
                                                    let _ = task.update(cx, |t, cx| t.compare_with(hash, None, window, cx));
                                                })
                                        }),
                                ),
                        )
                    })
                    .child(
                        div().flex_1().min_h_0().child(
                            // Wrapped: unwrapped rows lose the vertical scrollbar here.
                            Diff::new(&self.diff)
                                .line_number(true)
                                .soft_wrap(true)
                                // The size of the Changes tab's diff: Ctrl+wheel there and the presentation's size reach this one too.
                                .code_font_size(crate::zoom::size(crate::zoom::Kind::Changes, cx))
                                .render_header_suffix({
                                    let (root, task, lines) = (self.root.clone(), self.task.clone(), self.first_lines.clone());
                                    move |file, _, _| crate::task::open_file_button(&root, &task, file, &lines)
                                })
                                .on_line_click(move |position, event, window, cx| {
                                    if !event.modifiers().secondary(cx) {
                                        return;
                                    }
                                    // Opens the file as it is now, at that line.
                                    let path = root.join(position.path().as_ref());
                                    let line = position.line().saturating_sub(1) as u32;
                                    let _ = task.update(cx, |t, cx| t.open_file(path, line, window, cx));
                                }),
                        ),
                    ),
            )
    }
}

/// The lanes' colors, and gray for those past the last drawn.
fn palette(cx: &App) -> [Hsla; 9] {
    let dark = cx.theme().mode.is_dark();
    let l = if dark { 0.62 } else { 0.45 };
    let mut colors = [hsla(0., 0., 0.5, 1.); 9];
    for (i, hue) in [0.58, 0.08, 0.9, 0.15, 0.75, 0.45, 0.0, 0.3].into_iter().enumerate() {
        colors[i] = hsla(hue, 0.7, l, 1.);
    }
    colors
}

/// A row of the graph: the branches and tags at the commit, its lanes, its
/// subject, author, date and hash.
#[allow(clippy::too_many_arguments)]
fn graph_row(
    ix: usize,
    commit: &git::Commit,
    row: Option<(Rc<Vec<graph::Row>>, usize)>,
    lanes: usize,
    palette: [Hsla; 9],
    selected: bool,
    // Not among those searched for: faint, the lanes as they are.
    dim: bool,
    jump: Option<(usize, SharedString)>,
    owner: WeakEntity<HistoryView>,
    cx: &App,
) -> AnyElement {
    // A merge has seldom anything to read: its whole row is dimmer.
    let muted = cx.theme().muted_foreground;
    let text = match (dim, commit.merge) {
        (true, _) => muted.opacity(0.5),
        (false, true) => muted,
        (false, false) => cx.theme().foreground,
    };
    let color = row.as_ref().map_or(palette[8], |(rows, i)| lane_color(&palette, rows[*i].at, rows[*i].color));
    let (first, more) = first_ref(&commit.refs).map_or((None, 0), |(name, more)| (Some(name.to_string()), more));
    ListItem::new(ix)
        .selected(selected)
        .h(GRAPH_ROW)
        .py_0()
        .px_2()
        .w_full()
        .child(
            h_flex()
                .gap_3()
                .w_full()
                .min_w_0()
                .h_full()
                .text_xs()
                .child(
                    h_flex()
                        .id(("graph-refs", ix))
                        .when(more > 0, |refs| {
                            let all = all_refs(&commit.refs);
                            refs.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(all.clone()).build(window, cx))
                        })
                        .w(px(130.))
                        .flex_shrink(1.)
                        .min_w(px(60.))
                        .gap_1()
                        .overflow_hidden()
                        .when_some(first, |refs, name| {
                            refs.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .px_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(color.opacity(0.6))
                                    .text_color(color)
                                    .child(name.trim_start_matches("tag: ").to_string()),
                            )
                            .when(more > 0, |refs| refs.child(div().flex_none().text_color(muted).child(format!("+{more}"))))
                        }),
                )
                .child({
                    let lane = (GRAPH_WIDTH / lanes.max(1) as f32).min(LANE).max(NARROW_LANE);
                    let owner = owner.clone();
                    div()
                        .id(("graph-lanes", ix))
                        .w(lane * lanes as f32)
                        .h(GRAPH_ROW)
                        .flex_none()
                        .when_some(row, |cell, (rows, i)| {
                            cell.child(canvas(|_, _, _| (), move |bounds, _, window, _| paint_lanes(&rows[i], bounds, lane, &palette, window)).size_full())
                        })
                        .when_some(jump, |cell, (to, told)| {
                            cell.cursor_pointer()
                                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(told.clone()).build(window, cx))
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    let _ = owner.update(cx, |this, cx| this.jump_to(to, cx));
                                })
                        })
                })
                .child(div().flex_1().min_w(px(160.)).truncate().text_sm().text_color(text).child(commit.subject.clone()))
                .child(div().w(px(100.)).flex_shrink(1.).min_w(px(40.)).truncate().text_color(text.opacity(0.85)).child(commit.author.clone()))
                .child(div().w(px(80.)).flex_none().truncate().text_color(text.opacity(0.85)).child(commit.date.clone()))
                .child(div().w(px(76.)).flex_none().whitespace_nowrap().overflow_hidden().font_family(cx.theme().mono_font_family.clone()).text_color(muted).child(commit.short.clone()))
                .child(div().w(px(6.)).flex_none()),
        )
        .on_click(move |_, _, cx| {
            let _ = owner.update(cx, |this, cx| this.select(ix, cx));
        })
        .into_any_element()
}

/// The branch or tag a row shows of those at a commit: one's own branches
/// first, as shorter and the ones looked for. With how many more there are.
fn first_ref(refs: &str) -> Option<(&str, usize)> {
    let all: Vec<&str> = refs.split(", ").filter(|name| !name.is_empty()).collect();
    let first = all.iter().find(|r| !r.contains('/') && !r.starts_with("tag: ")).or(all.first())?;
    Some((first, all.len() - 1))
}

/// Every branch and tag at a commit, a line each: the tooltip of its "+N".
fn all_refs(refs: &str) -> SharedString {
    refs.split(", ").map(|r| r.trim_start_matches("tag: ")).collect::<Vec<_>>().join("\n").into()
}

/// The list's badge for the branches and tags at a commit: the first, cut
/// short when long, "+N" for the rest, and all of them on hover.
fn ref_badges(ix: usize, commit: &git::Commit, cx: &App) -> Option<AnyElement> {
    let (name, more) = first_ref(&commit.refs)?;
    let tag = name.starts_with("tag: ");
    let all = all_refs(&commit.refs);
    Some(
        h_flex()
            .id(("history-refs", ix))
            .flex_none()
            .max_w(px(150.))
            .gap_1()
            .text_xs()
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(all.clone()).build(window, cx))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .px_1()
                    .rounded_sm()
                    .bg(cx.theme().accent)
                    .text_color(if tag { gpui_kit::yellow() } else { cx.theme().primary })
                    .child(name.trim_start_matches("tag: ").to_string()),
            )
            .when(more > 0, |refs| refs.child(div().flex_none().text_color(cx.theme().muted_foreground).child(format!("+{more}"))))
            .into_any_element(),
    )
}

fn lane_color(palette: &[Hsla; 9], lane: usize, color: usize) -> Hsla {
    if lane >= graph::MOST_LANES { palette[8] } else { palette[color % 8] }
}

/// What a row of the graph draws: the lanes passing, those ending in the
/// commit from above and leaving it below, bent from lane to lane, and its dot.
fn paint_lanes(row: &graph::Row, bounds: Bounds<Pixels>, width: Pixels, palette: &[Hsla; 9], window: &mut Window) {
    let x = |lane: usize| bounds.left() + width * lane.min(graph::MOST_LANES - 1) as f32 + width / 2.;
    let (top, mid, bottom) = (bounds.top(), bounds.top() + bounds.size.height / 2., bounds.bottom());
    let stroke = |from: Point<Pixels>, to: Point<Pixels>, bend_at: Pixels, color: Hsla, window: &mut Window| {
        let mut path = PathBuilder::stroke(px(1.5));
        path.move_to(from);
        if from.x == to.x {
            path.line_to(to);
        } else {
            path.curve_to(to, point(bend_at, mid));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    };
    for line in &row.through {
        stroke(point(x(line.lane), top), point(x(line.lane), bottom), x(line.lane), lane_color(palette, line.lane, line.color), window);
    }
    for line in &row.into {
        stroke(point(x(line.lane), top), point(x(row.at), mid), x(line.lane), lane_color(palette, line.lane, line.color), window);
    }
    for line in &row.out {
        stroke(point(x(row.at), mid), point(x(line.lane), bottom), x(line.lane), lane_color(palette, line.lane, line.color), window);
    }
    // Long lines: a short end each, with an arrow toward where it goes on.
    let arrow = |tip: Point<Pixels>, down: bool, color: Hsla, window: &mut Window| {
        let back = if down { -px(4.) } else { px(4.) };
        let mut head = PathBuilder::stroke(px(1.5));
        head.move_to(point(tip.x - px(3.), tip.y + back));
        head.line_to(tip);
        head.line_to(point(tip.x + px(3.), tip.y + back));
        if let Ok(head) = head.build() {
            window.paint_path(head, color);
        }
    };
    for line in &row.down {
        let (color, tip) = (lane_color(palette, line.lane, line.color), point(x(line.lane), bottom - px(3.)));
        stroke(point(x(row.at), mid), tip, x(line.lane), color, window);
        arrow(tip, true, color, window);
    }
    for line in &row.up {
        let (color, tip) = (lane_color(palette, line.lane, line.color), point(x(line.lane), top + px(3.)));
        stroke(tip, point(x(row.at), mid), x(line.lane), color, window);
        arrow(tip, false, color, window);
    }
    let r = if row.merge { px(2.5) } else { px(3.5) };
    let dot = Bounds::new(point(x(row.at) - r, mid - r), size(r * 2., r * 2.));
    window.paint_quad(fill(dot, lane_color(palette, row.at, row.color)).corner_radii(r));
}
