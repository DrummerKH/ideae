use super::*;

// ---------------------------------------------------------------- changes

#[derive(Clone, Copy, PartialEq)]
pub enum DiffScope {
    /// The worktree against HEAD.
    Uncommitted,
    /// The worktree against where the branch forked from its base.
    Branch,
}

pub struct ChangesView {
    pub(super) root: PathBuf,
    pub diff: Entity<DiffState>,
    pub(super) scope: DiffScope,
    /// File counts for both scopes, and the base branch, for the toggle.
    pub(super) counts: (usize, usize),
    pub(super) base: Option<String>,
    /// First changed line per file in the diff shown, for "open".
    pub(super) first_lines: Rc<HashMap<String, u32>>,
    /// The single changes of the diff shown: each can be taken back or
    /// staged without the rest of its file.
    pub(super) hunks: Rc<Vec<git::Hunk>>,
    /// Those of them that are staged, by file and lines.
    pub(super) staged: Rc<HashSet<(String, String)>>,
    pub(super) selection: Option<DiffLineRange>,
    pub(super) task: WeakEntity<TaskView>,
    /// The branches to compare with, for the menu: listed off the UI thread.
    pub(super) branches: Rc<std::cell::RefCell<Vec<String>>>,
    /// Review comments on lines of the diff, sent to the agent as one message.
    pub(super) review: Vec<crate::state::ReviewNote>,
    /// The text of the last selection: a click in the context menu clears
    /// the selection before its action runs.
    pub(super) selected_code: String,
    /// Each shown file's part of the patch, hashed: what the selection was made in.
    pub(super) shown_keys: Rc<HashMap<String, u64>>,
}

/// Lists the repository's branches, newest first, into `known`.
pub(super) fn load_branches<T: 'static>(root: PathBuf, known: Rc<std::cell::RefCell<Vec<String>>>, cx: &mut Context<T>) {
    cx.spawn(async move |_, cx| {
        let found = cx.background_spawn(async move { git::branches(&root).into_iter().take(40).collect::<Vec<_>>() }).await;
        *known.borrow_mut() = found;
    })
    .detach();
}

impl ChangesView {
    pub(super) fn new(root: PathBuf, task: WeakEntity<TaskView>, cx: &mut Context<Self>) -> Self {
        let mode = if crate::state::with_settings(|s| s.diff_unified) { DiffMode::Unified } else { DiffMode::Split };
        let diff = cx.new(|cx| DiffState::new([], cx).with_mode(mode));
        cx.observe(&diff, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&diff, |this, _, event: &DiffEvent, cx| match event {
            DiffEvent::SelectionEnded(range) => {
                this.selection = Some(range.clone());
                this.selected_code = this.diff.read(cx).selected_text(cx);
                cx.notify();
            }
            // Not cleared on SelectionChanged(None): pressing the Open button
            // clears the diff selection, which would remove the button mid-click.
            _ => {}
        })
        .detach();
        let branches = Rc::default();
        load_branches(root.clone(), Rc::clone(&branches), cx);
        Self {
            root,
            diff,
            scope: DiffScope::Uncommitted,
            counts: (0, 0),
            base: None,
            first_lines: Rc::default(),
            selection: None,
            task,
            review: Vec::new(),
            selected_code: String::new(),
            shown_keys: Rc::default(),
            branches,
            hunks: Rc::default(),
            staged: Rc::default(),
        }
    }

    /// The selected lines: where they are, their text, their language.
    pub(super) fn selected(&self, cx: &App) -> Option<(String, String, &'static str)> {
        let range = self.selected_range(cx)?;
        let mut code = self.diff.read(cx).selected_text(cx);
        if code.is_empty() {
            code = self.selected_code.clone();
        }
        if code.is_empty() {
            return None;
        }
        Some((location_of(&range), code, lsp::language_for(std::path::Path::new(range.path().as_ref()))))
    }

    pub(super) fn selected_range(&self, cx: &App) -> Option<DiffLineRange> {
        self.diff.read(cx).selected_lines().or(self.selection.clone())
    }

    /// A review note for `range`: the selected text when that is the range,
    /// else the lines read from the file (the changed side).
    pub(super) fn note_for(&self, range: &DiffLineRange, cx: &App) -> crate::state::ReviewNote {
        let modified = range.end_side() == DiffSide::Modified;
        let mut code = String::new();
        if self.selected_range(cx).as_ref() == Some(range) {
            code = self.diff.read(cx).selected_text(cx);
            if code.is_empty() {
                code = self.selected_code.clone();
            }
        }
        if code.is_empty() && modified {
            if let Ok(text) = std::fs::read_to_string(self.root.join(range.path().as_ref())) {
                let (a, b) = (range.start().min(range.end()), range.start().max(range.end()));
                code = text.lines().skip(a.saturating_sub(1)).take(b + 1 - a.max(1)).collect::<Vec<_>>().join("\n");
            }
        }
        crate::state::ReviewNote {
            location: location_of(range),
            code,
            language: lsp::language_for(std::path::Path::new(range.path().as_ref())).to_string(),
            text: String::new(),
            path: range.path().to_string(),
            modified_side: modified,
            line: range.end(),
        }
    }

    /// Asks for a comment on the selected lines and adds it to the review.
    pub(super) fn comment_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.selected_range(cx) else { return };
        let note = self.note_for(&range, cx);
        self.ask_comment(note, window, cx);
    }

    pub(super) fn ask_comment(&mut self, note: crate::state::ReviewNote, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx).auto_grow(3, 12).placeholder("What should change here?")
        });
        let view = cx.weak_entity();
        let focused = Rc::new(std::cell::Cell::new(false));
        let title = note.location.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, view, focused) = (input.clone(), view.clone(), focused.clone());
            let note = note.clone();
            let shown = input.clone();
            let add = move |window: &mut Window, cx: &mut App| {
                let text = input.read(cx).value().trim().to_string();
                if text.is_empty() {
                    return;
                }
                let comment = crate::state::ReviewNote { text, ..note.clone() };
                let _ = view.update(cx, |c, cx| {
                    c.review.push(comment);
                    cx.notify();
                });
                window.close_dialog(cx);
            };
            dialog
                .title(format!("Comment on {title}"))
                .w(px(560.))
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let input = shown.clone();
                        window.defer(cx, move |window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
                    }
                    content.child(gpui_kit::component::input::Textarea::new(&shown))
                })
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("review-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("review-add").primary().label("Add comment").on_click(move |_, window, cx| add(window, cx))),
                )
        });
    }

    /// Every review comment to the agent in one message, pasted for the user
    /// to send; the review starts over.
    pub(super) fn send_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_empty() {
            return;
        }
        let mut message = String::from("Review comments on the changes:\n");
        for (i, c) in self.review.iter().enumerate() {
            message.push_str(&format!(
                "\n{}. {}\n```{}\n{}\n```\n{}\n",
                i + 1,
                c.location,
                c.language,
                c.code.trim_end_matches('\n'),
                c.text
            ));
        }
        self.review.clear();
        cx.notify();
        // Once this view's own update is over: the task may show this view
        // a newer diff on the way, which reads it.
        let task = self.task.clone();
        window.defer(cx, move |window, cx| {
            let _ = task.update(cx, |t, cx| t.paste_to_agent(message, window, cx));
        });
    }

    pub(super) fn render_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.review.is_empty() {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        Some(
            v_flex()
                .px_3()
                .py_2()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary.opacity(0.5))
                .child(
                    h_flex()
                        .justify_between()
                        .child(div().text_xs().text_color(muted).child(format!("REVIEW ({})", self.review.len())))
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("review-clear")
                                        .xsmall()
                                        .ghost()
                                        .label("Clear")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.review.clear();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("review-send")
                                        .xsmall()
                                        .primary()
                                        .label("Send to agent")
                                        .on_click(cx.listener(|this, _, window, cx| this.send_review(window, cx))),
                                ),
                        ),
                )
                .children(self.review.iter().enumerate().map(|(i, c)| {
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(div().flex_none().font_family(cx.theme().mono_font_family.clone()).text_xs().text_color(muted).child(c.location.clone()))
                        .child(div().flex_1().min_w_0().truncate().child(c.text.clone()))
                        .child(
                            Button::new(("review-remove", i))
                                .xsmall()
                                .ghost()
                                .label("×")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if i < this.review.len() {
                                        this.review.remove(i);
                                    }
                                    cx.notify();
                                })),
                        )
                }))
                .into_any_element(),
        )
    }

    /// The lines selected by their numbers, to the agent with their location.
    pub(super) fn send_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((location, code, language)) = self.selected(cx) else { return };
        let task = self.task.clone();
        window.defer(cx, move |window, cx| {
            let _ = task.update(cx, |t, cx| t.send_to_agent(location, code, language, window, cx));
        });
    }

    pub(super) fn set_scope(&mut self, scope: DiffScope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        // Read from the task rather than asking it to push: it would read this
        // view back while it is being updated.
        let Ok((files, lines, hunks)) = self.task.read_with(cx, |task, _| match scope {
            DiffScope::Uncommitted => (task.uncommitted_diff.clone(), task.uncommitted_lines.clone(), task.uncommitted_hunks.clone()),
            DiffScope::Branch => (task.branch_diff.clone(), task.branch_lines.clone(), task.branch_hunks.clone()),
        }) else {
            return;
        };
        self.first_lines = lines;
        self.hunks = hunks;
        self.diff.update(cx, |d, cx| d.set_files(files, cx));
        // The other scope's files: those viewed fold there too.
        let diff = self.diff.clone();
        let task = self.task.clone();
        cx.defer(move |cx| {
            let _ = task.update(cx, |task, cx| {
                task.folded_viewed.clear();
                task.fold_viewed(&diff, cx);
            });
        });
        cx.notify();
    }

    pub(super) fn open_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.selection.clone() else { return };
        let path = self.root.join(range.path().as_ref());
        let line = range.start().saturating_sub(1) as u32;
        let task = self.task.clone();
        window.defer(cx, move |window, cx| {
            let _ = task.update(cx, |task, cx| task.open_file(path, line, window, cx));
        });
    }
}

impl Render for ChangesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Split or unified is one choice for every task's Changes, kept over a restart.
        let wanted = if crate::state::with_settings(|s| s.diff_unified) { DiffMode::Unified } else { DiffMode::Split };
        if self.diff.read(cx).mode() != wanted {
            self.diff.update(cx, |d, cx| d.set_mode(wanted, cx));
        }
        let mode = wanted;
        let empty = self.diff.read(cx).files().is_empty();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("scope-uncommitted")
                            .small()
                            .ghost()
                            .label(format!("Uncommitted ({})", self.counts.0))
                            .selected(self.scope == DiffScope::Uncommitted)
                            .on_click(cx.listener(|this, _, _, cx| this.set_scope(DiffScope::Uncommitted, cx))),
                    )
                    .child(
                        Button::new("scope-branch")
                            .small()
                            .ghost()
                            .disabled(self.base.is_none())
                            .label(match &self.base {
                                Some(base) => format!("Branch vs {base} ({})", self.counts.1),
                                None => "Branch (no base found)".to_string(),
                            })
                            .selected(self.scope == DiffScope::Branch)
                            .on_click(cx.listener(|this, _, _, cx| this.set_scope(DiffScope::Branch, cx))),
                    )
                    .child({
                        let (root, task, view, known) = (self.root.clone(), self.task.clone(), cx.weak_entity(), self.branches.clone());
                        Button::new("pick-base")
                            .small()
                            .ghost()
                            .label("▾")
                            .tooltip("Compare with another branch")
                            .dropdown_menu(move |mut menu, _, cx| {
                                // As listed last; asked again for the next time it opens.
                                load_branches(root.clone(), known.clone(), cx);
                                let branches = known.borrow().clone();
                                let direct = task.clone();
                                menu = menu
                                    .item(gpui_kit::component::menu::PopupMenuItem::new("With a branch, tag or commit as it is…").on_click(move |_, window, cx| {
                                        let _ = direct.update(cx, |t, cx| t.compare_with_dialog(None, window, cx));
                                    }))
                                    .separator()
                                    .label("Since the branch left");
                                for branch in branches {
                                    let (task, view, name) = (task.clone(), view.clone(), branch.clone());
                                    menu = menu.item(gpui_kit::component::menu::PopupMenuItem::new(branch).on_click(
                                        move |_, _, cx| {
                                            let _ = task.update(cx, |t, _| t.set_base(name.clone()));
                                            let _ = view.update(cx, |c, cx| c.set_scope(DiffScope::Branch, cx));
                                        },
                                    ));
                                }
                                menu
                            })
                    })
                    .child(div().w(px(1.)).h(px(16.)).bg(cx.theme().border))
                    // Both named, the one in use lit: one word alone read as a label, not as a switch.
                    .children([("mode-split", "Split", DiffMode::Split, "Old and new side by side"), ("mode-unified", "Unified", DiffMode::Unified, "Old and new one under the other")].map(
                        |(id, label, to, tip)| {
                            Button::new(id).small().ghost().label(label).tooltip(tip).selected(mode == to).on_click(cx.listener(move |this, _, _, cx| {
                                let mut settings = crate::state::settings();
                                settings.diff_unified = to == DiffMode::Unified;
                                crate::state::set_settings(settings);
                                this.diff.update(cx, |d, cx| d.set_mode(to, cx));
                            }))
                        },
                    ))
                    .child(div().w(px(1.)).h(px(16.)).bg(cx.theme().border))
                    .children([("fold-all", "Collapse all", true, "Every file to its header"), ("unfold-all", "Expand all", false, "Every file open, those viewed too")].map(
                        |(id, label, folded, tip)| {
                            Button::new(id).small().ghost().label(label).tooltip(tip).on_click(cx.listener(move |this, _, _, cx| {
                                this.diff.update(cx, |d, cx| d.set_all_collapsed(folded, cx));
                            }))
                        },
                    ))
                    .child(
                        div()
                            .id("changes-hint")
                            .ml_auto()
                            .px_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("ⓘ")
                            .tooltip(|window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(format!(
                                    "{} a line to open the file there. Select lines by their numbers to comment on them or send them to the agent (right-click).",
                                    crate::keys::label("Ctrl+Click")
                                ))
                                .build(window, cx)
                            }),
                    )
                    .when_some(self.selection.clone(), |this, range| {
                        this.child(
                            Button::new("open-at-line")
                                .small()
                                .primary()
                                .label(format!("Open {}:{}", range.path(), range.start()))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_selection(window, cx)
                                })),
                        )
                    }),
            )
            .children(self.render_review(cx))
            .child(div().id("changes-diff").relative().flex_1().min_h_0()
                .child(crate::zoom::wheel(crate::zoom::Kind::Changes))
                .on_action(cx.listener(|this, _: &SendDiffToAgent, window, cx| this.send_selection(window, cx)))
                .on_action(cx.listener(|this, _: &CommentOnDiff, window, cx| this.comment_selection(window, cx)))
                .context_menu(|menu, _, _| {
                    menu.menu("Comment on selected lines…", Box::new(CommentOnDiff))
                        .menu("Send selected lines to Agent", Box::new(SendDiffToAgent))
                })
                .map(|this| {
                if empty {
                    this.flex()
                        .items_center()
                        .justify_center()
                        .text_color(cx.theme().muted_foreground)
                        .child("No uncommitted changes")
                } else {
                    let notes = Rc::new(self.review.clone());
                    let view = cx.weak_entity();
                    let add_view = cx.weak_entity();
                    let staging = self.scope == DiffScope::Uncommitted;
                    this.child(
                        Diff::new(&self.diff)
                            .annotations(
                                notes
                                    .iter()
                                    .enumerate()
                                    .map(|(i, n)| {
                                        let side = if n.modified_side { DiffSide::Modified } else { DiffSide::Original };
                                        DiffAnnotation::line(("review-note", i), DiffLinePosition::new(n.path.clone(), side, n.line))
                                    })
                                    .collect::<Vec<_>>(),
                            )
                            .render_annotation(move |annotation, _, cx| {
                                let i = match annotation.id() {
                                    ElementId::NamedInteger(_, i) => *i as usize,
                                    _ => usize::MAX,
                                };
                                let text = notes.get(i).map(|n| n.text.clone()).unwrap_or_default();
                                let view = view.clone();
                                h_flex()
                                    .mx_2()
                                    .my_1()
                                    .px_3()
                                    .py_1p5()
                                    .gap_2()
                                    .rounded_md()
                                    .border_l_2()
                                    .border_color(cx.theme().primary)
                                    .bg(cx.theme().secondary)
                                    .text_sm()
                                    .child(div().flex_1().min_w_0().child(text))
                                    .child(Button::new(("review-note-remove", i)).xsmall().ghost().label("×").on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |c, cx| {
                                            if i < c.review.len() {
                                                c.review.remove(i);
                                            }
                                            cx.notify();
                                        });
                                    }))
                                    .into_any_element()
                            })
                            .on_add_annotation(move |range, window, cx| {
                                let _ = add_view.update(cx, |c, cx| {
                                    let note = c.note_for(range, cx);
                                    c.ask_comment(note, window, cx);
                                });
                            })
                            .code_font_size(crate::zoom::size(crate::zoom::Kind::Changes, cx))
                            .line_number(true)
                            .render_header_suffix({
                                let (root, task, lines) = (self.root.clone(), self.task.clone(), self.first_lines.clone());
                                move |file, _, cx| {
                                    h_flex()
                                        .gap_2()
                                        .when(staging, |row| row.child(viewed_checkbox(&task, file, cx)))
                                        .child(open_file_button(&root, &task, file, &lines))
                                }
                            })
                            .render_header_status({
                                let task = self.task.clone();
                                move |file, _, cx| {
                                    let task = task.upgrade()?;
                                    let untracked = task.read(cx).file_states.get(file.path().as_ref()).is_some_and(|s| s.change == git::Change::Untracked);
                                    untracked.then(|| SharedString::new_static("Untracked"))
                                }
                            })
                            // What can be done with each change alone, on the row of its hunk.
                            .render_hunk_suffix({
                                let (hunks, task, in_index) = (self.hunks.clone(), self.task.clone(), self.staged.clone());
                                move |file, original, modified, _, _| hunk_actions(file, original, modified, &hunks, &task, &in_index, staging)
                            })
                            .on_line_click({
                                let (root, task) = (self.root.clone(), self.task.clone());
                                move |position, event, window, cx| {
                                    // Ctrl+click opens; a plain click stays in the review.
                                    if !event.modifiers().secondary(cx) {
                                        return;
                                    }
                                    let path = root.join(position.path().as_ref());
                                    let line = position.line().saturating_sub(1) as u32;
                                    let task = task.clone();
                                    // After the click is done with the diff, which
                                    // opening the file may give other files.
                                    window.defer(cx, move |window, cx| {
                                        let _ = task.update(cx, |task, cx| task.open_file(path, line, window, cx));
                                    });
                                }
                            })
                            // Unwrapped split puts the new side past the right edge
                            // when any line is long, so split always wraps.
                            .soft_wrap(mode == DiffMode::Split),
                    )
                }
            }))
    }
}

/// The first and last line a change covers on the side `line` is on.
fn change_lines(hunk: &git::Hunk) -> (u32, u32) {
    let sign = if hunk.modified { '+' } else { '-' };
    let count = hunk.changes.lines().filter(|l| l.starts_with(sign)).count().max(1) as u32;
    (hunk.line.saturating_sub(count - 1), hunk.line)
}

/// Revert and Stage for the changes in one hunk of the diff: buttons for
/// the one change it has, menus naming their lines for several.
fn hunk_actions(
    file: &DiffFile,
    original: std::ops::Range<usize>,
    modified: std::ops::Range<usize>,
    hunks: &Rc<Vec<git::Hunk>>,
    task: &WeakEntity<TaskView>,
    in_index: &Rc<HashSet<(String, String)>>,
    staging: bool,
) -> Option<AnyElement> {
    use gpui_kit::component::menu::PopupMenuItem;
    let path = file.path().as_ref();
    let inside: Vec<usize> = hunks
        .iter()
        .enumerate()
        .filter(|(_, h)| h.path == path && (if h.modified { &modified } else { &original }).contains(&(h.line as usize)))
        .map(|(i, _)| i)
        .collect();
    let &first = inside.first()?;
    let is_staged = |h: &git::Hunk| in_index.contains(&(h.path.clone(), h.changes.clone()));
    const REVERT_TIP: &str = "Take this change back; the rest of the file stays (Undo follows)";
    let mut row = h_flex().flex_none().gap_0p5();
    if inside.len() == 1 {
        let hunk = hunks[first].clone();
        let staged = is_staged(&hunk);
        let (revert, stage, staged_hunk) = (task.clone(), task.clone(), hunk.clone());
        row = row.child(Button::new(("hunk-revert", first)).xsmall().ghost().label("↺ Revert").tooltip(REVERT_TIP).on_click(move |_, window, cx| {
            let _ = revert.update(cx, |t, cx| t.revert_hunk(hunk.clone(), window, cx));
        }));
        if staging {
            let (label, tip) = if staged { ("− Unstage", "Take this change out of what is staged") } else { ("+ Stage", "Stage this change alone") };
            row = row.child(Button::new(("hunk-stage", first)).xsmall().ghost().label(label).tooltip(tip).on_click(move |_, window, cx| {
                let _ = stage.update(cx, |t, cx| t.stage_hunk(staged_hunk.clone(), !staged, window, cx));
            }));
        }
        return Some(row.into_any_element());
    }
    let named = |h: &git::Hunk| match change_lines(h) {
        (a, b) if a == b => format!("Line {b}"),
        (a, b) => format!("Lines {a}–{b}"),
    };
    let (revert_hunks, revert_task) = (hunks.clone(), task.clone());
    let revert_inside = inside.clone();
    row = row.child(Button::new(("hunk-revert", first)).xsmall().ghost().label("↺ Revert ▾").tooltip("Take one of the changes here back; the rest of the file stays (Undo follows)").dropdown_menu(
        move |mut menu, _, _| {
            for &i in &revert_inside {
                let (hunk, task) = (revert_hunks[i].clone(), revert_task.clone());
                menu = menu.item(PopupMenuItem::new(named(&hunk)).on_click(move |_, window, cx| {
                    let _ = task.update(cx, |t, cx| t.revert_hunk(hunk.clone(), window, cx));
                }));
            }
            menu
        },
    ));
    if staging {
        let (stage_hunks, stage_task, in_index) = (hunks.clone(), task.clone(), in_index.clone());
        row = row.child(Button::new(("hunk-stage", first)).xsmall().ghost().label("Stage ▾").tooltip("Stage or unstage one of the changes here alone").dropdown_menu(move |mut menu, _, _| {
            for &i in &inside {
                let (hunk, task) = (stage_hunks[i].clone(), stage_task.clone());
                let staged = in_index.contains(&(hunk.path.clone(), hunk.changes.clone()));
                let label = format!("{} {}", if staged { "Unstage" } else { "Stage" }, named(&hunk).to_lowercase());
                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    let _ = task.update(cx, |t, cx| t.stage_hunk(hunk.clone(), !staged, window, cx));
                }));
            }
            menu
        }));
    }
    Some(row.into_any_element())
}
