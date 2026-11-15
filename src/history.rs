use std::{path::PathBuf, rc::Rc};

use gpui_kit::component::{
    ActiveTheme as _, StyledExt as _,
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
    _load: Task<()>,
    _show: Task<()>,
}

/// The whole history: `git log` of 6k commits takes 30 ms and the list is
/// virtualized. The cap only guards against giant monorepos.
const LIMIT: usize = 200_000;

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
            _load: Task::ready(()),
            _show: Task::ready(()),
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
        let root = self.root.clone();
        self._load = cx.spawn(async move |this, cx| {
            let commits = cx.background_spawn(async move { git::history(&root, LIMIT) }).await;
            let _ = this.update(cx, |this, cx| {
                // Keep the selection on the same commit when it is still listed.
                let selected_hash = this.selected.and_then(|ix| this.commits.get(ix)).map(|c| c.hash.clone());
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

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(match self.commits.len() { 1 => "1 commit".to_string(), n => format!("{n} commits") }),
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
                                                .child(div().text_sm().truncate().child(commit.subject.clone()))
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.stale {
            self.load(cx);
        }
        let commit = self.selected.and_then(|ix| self.commits.get(ix)).cloned();
        let root = self.root.clone();
        let task = self.task.clone();
        h_flex()
            .size_full()
            .child(self.render_list(cx))
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
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!("{} · {} · {}", c.short, c.author, c.date)),
                                ),
                        )
                    })
                    .child(
                        div().flex_1().min_h_0().child(
                            // Wrapped: unwrapped rows lose the vertical scrollbar here.
                            Diff::new(&self.diff)
                                .line_number(true)
                                .soft_wrap(true)
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
