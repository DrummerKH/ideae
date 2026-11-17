use super::*;

impl TaskView {
    /// Opens History on one commit (from a blame line).
    /// The repository's branches, to choose one and say what to do with
    /// it; and all the merged ones at once, to tidy up.
    pub fn branches_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const MERGED: &str = "\u{1}merged";
        let (root, task, here) = (self.root.clone(), cx.weak_entity(), self.branch_info.head.clone());
        cx.spawn_in(window, async move |_, cx| {
            let from = root.clone();
            let mut rows = cx.background_spawn(async move { git::branch_rows(&from) }).await;
            // Local ones first, then those of the remote; each as git lists them.
            rows.sort_by_key(|row| row.remote);
            let detail = crate::finder::DETAIL;
            // Merged, here, in no task and not the one in hand: nothing is lost with these.
            let spent: Vec<git::BranchRow> = rows.iter().filter(|r| r.merged && !r.remote && r.held.is_none() && r.name != here).cloned().collect();
            let detail_of = |row: &git::BranchRow| branch_detail(row, &here);
            let mut items = Vec::new();
            if !spent.is_empty() {
                let text = format!("Delete the {} merged local branch{}…{detail}all of what they hold is in the branch work starts from", spent.len(), if spent.len() == 1 { "" } else { "es" });
                items.push(crate::finder::Hit { text, path: MERGED.into(), line: 0, col: 0, ranges: Vec::new() });
            }
            items.extend(rows.iter().map(|row| crate::finder::Hit { text: format!("{}{detail}{}", row.name, detail_of(row)), path: row.name.clone(), line: 0, col: 0, ranges: Vec::new() }));
            let picked = task.clone();
            let _ = cx.update(|window, cx| {
                crate::finder::Finder::open_pick(
                    "Branches · Enter: open as a task, compare or delete".into(),
                    root,
                    items,
                    Rc::new(move |name, window, cx| {
                        let _ = picked.update(cx, |task, cx| {
                            if name == MERGED {
                                return task.delete_branches_dialog(spent.clone(), window, cx);
                            }
                            if let Some(row) = rows.iter().find(|row| row.name == name) {
                                task.branch_dialog(row.clone(), window, cx)
                            }
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

    /// What can be done with one branch.
    pub(super) fn branch_dialog(&mut self, branch: git::BranchRow, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let here = branch.name == self.branch_info.head;
        window.open_dialog(cx, move |dialog, _, cx| {
            let (this, branch) = (this.clone(), branch.clone());
            let muted = cx.theme().muted_foreground;
            dialog.title(format!("⎇ {}", branch.name)).w(px(520.)).content(move |content, _, _| {
                let act = |id: &'static str, label: String, tip: &'static str, run: Rc<dyn Fn(&mut TaskView, git::BranchRow, &mut Window, &mut Context<TaskView>)>| {
                    let (this, branch) = (this.clone(), branch.clone());
                    Button::new(id).small().outline().label(label).tooltip(tip).on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let (run, branch) = (run.clone(), branch.clone());
                        let _ = this.update(cx, |task, cx| run(task, branch, window, cx));
                    })
                };
                let held = branch.held.as_ref().map(|path| path.display().to_string());
                content
                    .child(div().text_sm().text_color(muted).child(format!("{} · {}", branch.when, branch.subject)))
                    .children(held.clone().map(|path| div().text_xs().text_color(muted).child(format!("Checked out in {path}: a task has it."))))
                    .child(
                        h_flex()
                            .gap_2()
                            .pt_3()
                            .flex_wrap()
                            .when(held.is_none(), |row| {
                                row.child(act(
                                    "branch-open",
                                    "Open as task".into(),
                                    "A worktree of its own for this branch: it shows up as a task",
                                    Rc::new(|task, branch, window, cx| {
                                        let copy_env = crate::state::settings().copy_env;
                                        task.run_action("Open", move |root| git::branch_worktree(&root, &branch, copy_env).map(|dir| format!("Opened in {}", dir.display())), false, window, cx)
                                    }),
                                ))
                            })
                            .child(act(
                                "branch-compare",
                                "Compare with working tree".into(),
                                "What is here that the branch does not have",
                                Rc::new(|task, branch, window, cx| task.compare_with(branch.name, None, window, cx)),
                            ))
                            .when(held.is_none() && !here, |row| {
                                row.child(act(
                                    "branch-delete",
                                    if branch.remote { "Delete on the remote…".into() } else { "Delete…".into() },
                                    "Asked once more before it goes, unless nothing would be lost",
                                    Rc::new(|task, branch, window, cx| task.delete_branches_dialog(vec![branch], window, cx)),
                                ))
                            }),
                    )
            })
        });
    }

    /// Deletes branches: at once when they are local and merged, asked
    /// first when something goes that is nowhere else, or goes for everyone.
    pub(super) fn delete_branches_dialog(&mut self, branches: Vec<git::BranchRow>, window: &mut Window, cx: &mut Context<Self>) {
        let delete = |task: &mut TaskView, branches: Vec<git::BranchRow>, window: &mut Window, cx: &mut Context<TaskView>| {
            task.run_action(
                "Delete",
                move |root| {
                    let mut gone = 0;
                    for branch in &branches {
                        // What was asked about is deleted as it is; what is merged needs no force.
                        git::delete_branch(&root, branch, !branch.merged)?;
                        gone += 1;
                    }
                    Ok(format!("Deleted {gone} branch{}", if gone == 1 { "" } else { "es" }))
                },
                false,
                window,
                cx,
            )
        };
        let one_safe = branches.len() == 1 && branches[0].merged && !branches[0].remote;
        if one_safe {
            return delete(self, branches, window, cx);
        }
        let this = cx.weak_entity();
        let names = branches.iter().map(|b| b.name.clone()).collect::<Vec<_>>().join(", ");
        let why = match branches.as_slice() {
            [one] if one.remote => "It is deleted on the remote, for everyone who uses it.",
            [one] if !one.merged => "It is not merged: its commits are kept nowhere else.",
            _ => "All of what they hold is in the branch work starts from.",
        };
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, branches) = (this.clone(), branches.clone());
            dialog
                .confirm()
                .title(if branches.len() == 1 { format!("Delete {names}?") } else { format!("Delete {} branches?", branches.len()) })
                .description(if branches.len() == 1 { why.to_string() } else { format!("{names}\n\n{why}") })
                .ok_text("Delete")
                .on_ok(move |_, window, cx| {
                    let branches = branches.clone();
                    let _ = this.update(cx, |task, cx| delete(task, branches, window, cx));
                    true
                })
        });
    }

    /// Asks what to compare the worktree with, or `file` of it alone: a
    /// branch, a tag, one of the latest commits.
    pub fn compare_with_dialog(&mut self, file: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let (root, task) = (self.root.clone(), cx.weak_entity());
        cx.spawn_in(window, async move |_, cx| {
            let from = root.clone();
            let refs = cx.background_spawn(async move { git::refs(&from) }).await;
            let items = refs
                .into_iter()
                .map(|(name, what)| crate::finder::Hit { text: format!("{name}{}{what}", crate::finder::DETAIL), path: name, line: 0, col: 0, ranges: Vec::new() })
                .collect();
            let title = match &file {
                Some(file) => format!("Compare {file} with…"),
                None => "Compare the working tree with…".to_string(),
            };
            let picked = task.clone();
            let _ = cx.update(|window, cx| {
                crate::finder::Finder::open_pick(
                    title,
                    root,
                    items,
                    Rc::new(move |rev, window, cx| {
                        let file = file.clone();
                        let _ = picked.update(cx, |task, cx| task.compare_with(rev, file, window, cx));
                    }),
                    task,
                    window,
                    cx,
                )
            });
        })
        .detach();
    }

    /// What the worktree has that `rev` has not, in a diff of its own.
    pub fn compare_with(&mut self, rev: String, file: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.root.clone();
        // A commit's whole hash says no more than its first ten characters.
        let shown = if rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) { rev[..10].to_string() } else { rev.clone() };
        let named = shown.clone();
        let source: DiffSource = std::sync::Arc::new(move || match git::work_of(&root, &rev, file.as_deref()) {
            Some((patch, base)) => (format!("what {named} did since {}, with what is not committed there yet", &base[..base.len().min(10)]), patch),
            None => (
                match &file {
                    Some(file) => format!("{file}: {named} on the left, as it is now on the right"),
                    None => format!("{named} on the left, the working tree on the right"),
                },
                git::diff_against(&root, &rev, file.as_deref()),
            ),
        });
        self.show_diff_tab(format!("⇄ {shown}"), source, window, cx);
    }

    /// A diff in a tab of its own; one of the same name gives it its place.
    pub(super) fn show_diff_tab(&mut self, label: String, source: DiffSource, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let read = source.clone();
            let (what, files) = cx.background_spawn(async move { read_source(&read) }).await;
            let _ = this.update_in(cx, |task, window, cx| task.place_diff_tab(label, what, files, source, window, cx));
        })
        .detach();
    }

    fn place_diff_tab(&mut self, label: String, what: String, files: Vec<DiffFile>, source: DiffSource, window: &mut Window, cx: &mut Context<Self>) {
        if files.is_empty() {
            return window.push_notification(Notification::info(format!("{what}: nothing differs.")), cx);
        }
        let view = cx.new(|cx| DiffTab::new(what, files, source, cx));
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::Diff(_)) && t.title.as_ref() == label) {
            self.tabs[ix].kind = TabKind::Diff(view);
            return self.activate(ix, window, cx);
        }
        self.place_tab(TabItem::new(label, TabKind::Diff(view)), OpenAs::Kept, window, cx);
    }
}

/// What a branch is and holds, in the words of the list: whose it is, what
/// is done in it, when; on a second line what it changes or its last commit.
/// A branch with no commits of its own is "merged" to git even while an
/// agent works in it: then what it changes is told.
fn branch_detail(row: &git::BranchRow, here: &str) -> String {
    let mut what = Vec::new();
    if let Some(about) = row.about.as_ref().filter(|_| row.agent) {
        what.push(format!("subagent: {about}"));
    } else if row.agent {
        what.push("subagent".to_string());
    } else if row.name == here {
        what.push("this task".into());
    } else if let Some(held) = &row.held {
        what.push(format!("in {}", held.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
    } else {
        what.push(if row.remote { "remote" } else { "local" }.into());
    }
    let ahead = row.ahead.unwrap_or(0);
    if ahead > 0 {
        what.push(format!("{ahead} commit{} ahead", if ahead == 1 { "" } else { "s" }));
    }
    if !row.changed.is_empty() {
        let n = row.changed.len();
        what.push(format!("{}{n} file{} changed", if ahead == 0 { "nothing committed yet, " } else { "" }, if n == 1 { "" } else { "s" }));
    } else if ahead == 0 && row.merged {
        what.push(if row.held.is_some() { "nothing done yet".into() } else { "merged".into() });
    }
    // Its own last commit says what it did, and when; one it has not made is the base's.
    let own = ahead > 0 || row.ahead.is_none() || row.held.is_none();
    if own {
        what.push(row.when.clone());
    }
    let second = if !row.changed.is_empty() {
        const SHOWN: usize = 8;
        let more = row.changed.len().saturating_sub(SHOWN);
        format!("{}{}", row.changed.iter().take(SHOWN).cloned().collect::<Vec<_>>().join("  "), if more > 0 { format!("  +{more}") } else { String::new() })
    } else if own {
        row.subject.clone()
    } else {
        String::new()
    };
    if second.is_empty() { what.join(" · ") } else { format!("{}\n{second}", what.join(" · ")) }
}
