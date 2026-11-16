use std::{path::PathBuf, rc::Rc};

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    diff::{Diff, DiffFile, DiffMode, DiffState},
    h_flex,
    list::ListItem,
    scroll::Scrollbar,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use crate::{git, task::TaskView};

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
    _typing: Task<()>,
    _load: Task<()>,
    _show: Task<()>,
    /// A commit shown from elsewhere that the list does not reach: put back
    /// when the list is read again while it is the one looked at.
    brought: Option<git::Commit>,
}

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
            _typing: Task::ready(()),
            _load: Task::ready(()),
            _show: Task::ready(()),
            brought: None,
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
        let (root, limit, query) = (self.root.clone(), self.limit, git::HistoryQuery::typed(&self.rev, &self.typed));
        self._load = cx.spawn(async move |this, cx| {
            let commits = cx.background_spawn(async move { git::history(&root, limit, &query) }).await;
            let _ = this.update(cx, |this, cx| {
                // Keep the selection on the same commit when it is still listed.
                let selected_hash = this.selected.and_then(|ix| this.commits.get(ix)).map(|c| c.hash.clone());
                let mut commits = commits;
                // One brought from beyond the list is still the one looked at: it stays first.
                if let Some(brought) = this.brought.clone().filter(|b| selected_hash.as_ref() == Some(&b.hash)) {
                    if !commits.iter().any(|c| c.hash == brought.hash) {
                        commits.insert(0, brought);
                    }
                }
                this.commits = Rc::new(commits);
                let ix = selected_hash
                    .and_then(|h| this.commits.iter().position(|c| c.hash == h))
                    .or((!this.commits.is_empty()).then_some(0));
                this.selected = None;
                if let Some(ix) = ix {
                    this.select(ix, cx);
                }
                cx.notify();
            });
        });
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
                .background_spawn(async move {
                    let patch = git::commit_patch(&root, &hash);
                    (crate::task::with_languages(DiffFile::parse(&patch).unwrap_or_default()), git::first_changed_lines(&patch))
                })
                .await;
            let lines = Rc::new(lines);
            let _ = this.update(cx, |this, cx| {
                this.first_lines = lines;
                this.diff.update(cx, |d, cx| d.set_files(files, cx));
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
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("words  @author  file:path"));
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
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
                    this.stale = true;
                    cx.notify();
                });
            });
        })
        .detach();
        self.filter = Some(input.clone());
        input
    }

    fn render_list(&self, filter: Entity<InputState>, cx: &mut Context<Self>) -> impl IntoElement {
        let commits = self.commits.clone();
        let selected = self.selected;
        let owner = cx.weak_entity();
        v_flex()
            .w(relative(0.35))
            .min_w(px(200.))
            .max_w(px(340.))
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
                                Button::new("history-branch")
                                    .xsmall()
                                    .ghost()
                                    .label(if self.rev.is_empty() { "⎇ this branch ▾".to_string() } else { format!("⎇ {} ▾", self.rev) })
                                    .tooltip("The history of another branch")
                                    .on_click(cx.listener(|this, _, window, cx| this.pick_branch(window, cx))),
                            )
                            .child(div().flex_1().min_w_0().child(Input::new(&filter).xsmall())),
                    )
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
                                                        .child(div().text_sm().min_w_0().truncate().child(commit.subject.clone()))
                                                        // The branches and tags at this commit.
                                                        .children(commit.refs.split(", ").filter(|name| !name.is_empty()).take(3).map(|name| {
                                                            let tag = name.starts_with("tag: ");
                                                            div()
                                                                .flex_none()
                                                                .px_1()
                                                                .rounded_sm()
                                                                .text_xs()
                                                                .bg(cx.theme().accent)
                                                                .text_color(if tag { gpui_kit::yellow() } else { cx.theme().primary })
                                                                .child(name.trim_start_matches("tag: ").to_string())
                                                        })),
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

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.stale {
            self.load(cx);
        }
        let filter = self.filter_input(window, cx);
        let commit = self.selected.and_then(|ix| self.commits.get(ix)).cloned();
        let root = self.root.clone();
        let task = self.task.clone();
        h_flex()
            .size_full()
            .child(self.render_list(filter, cx))
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
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!("{} · {} · {}", c.short, c.author, c.date)),
                                        )
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
