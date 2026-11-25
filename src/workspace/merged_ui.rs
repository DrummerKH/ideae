//! Remote branches of your merged PRs: looked for now and then in the
//! background, offered in the project's menu, deleted in one push.

use std::{cell::RefCell, collections::HashSet, path::PathBuf, rc::Rc, time::Duration};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    notification::Notification,
    v_flex,
};
use gpui_kit::*;

use super::Workspace;
use crate::merged_branches;

/// How often a project's merged branches are looked for: each look asks
/// GitHub twice.
const EVERY: Duration = Duration::from_secs(30 * 60);

thread_local! {
    /// Branches origin refused to delete as protected, by project: not
    /// offered again in this run.
    static PROTECTED: RefCell<HashSet<(PathBuf, String)>> = RefCell::default();
}

impl Workspace {
    /// The branches checked out by a project's tasks: never offered.
    fn branches_in_use(&self, pi: usize) -> HashSet<String> {
        self.projects.get(pi).map(|p| p.tasks.iter().map(|t| t.branch.clone()).collect()).unwrap_or_default()
    }

    /// Looks again for the merged branches of the projects not looked at
    /// lately, or whose task's PR was just merged.
    pub(super) fn refresh_merged_branches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if crate::lsp::POWER_SAVING.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let mut due: Vec<(PathBuf, HashSet<String>)> = Vec::new();
        for pi in 0..self.projects.len() {
            // A remote project's once its host is connected: git runs there, its code host is asked here.
            let p = &self.projects[pi];
            if !p.values().pull_requests || !crate::hostfs::reachable(&p.path) || p.merged_checked.is_some_and(|at| at.elapsed() < EVERY) {
                continue;
            }
            self.projects[pi].merged_checked = Some(std::time::Instant::now());
            due.push((self.projects[pi].path.clone(), self.branches_in_use(pi)));
        }
        if due.is_empty() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_spawn(crate::procs::on_thread(move || due.into_iter().map(|(repo, in_use)| {
                    let found = merged_branches::find(&repo, &in_use);
                    (repo, found)
                }).collect::<Vec<_>>()))
                .await;
            let _ = this.update(cx, |this, cx| {
                for (repo, found) in found {
                    if let Some(p) = this.projects.iter_mut().find(|p| p.path == repo) {
                        let found: Vec<_> = PROTECTED.with(|known| {
                            let known = known.borrow();
                            found.into_iter().filter(|b| !known.contains(&(repo.clone(), b.branch.clone()))).collect()
                        });
                        if p.merged_branches != found {
                            p.merged_branches = found;
                            cx.notify();
                        }
                    }
                }
            });
        })
        .detach();
    }

    /// The merged branches of a project, each with a box to keep it.
    pub fn merged_branches_dialog(&mut self, pi: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(pi) else { return };
        let listed = p.merged_branches.clone();
        if listed.is_empty() || self.is_presenting() {
            return;
        }
        let (repo, name) = (p.path.clone(), p.name.clone());
        let chosen: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(vec![true; listed.len()]));
        let listed = Rc::new(listed);
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (listed, chosen, this, repo) = (listed.clone(), chosen.clone(), this.clone(), repo.clone());
            let count = chosen.borrow().iter().filter(|on| **on).count();
            let delete = {
                let (listed, chosen) = (listed.clone(), chosen.clone());
                move |window: &mut Window, cx: &mut App| {
                    let picked: Vec<String> = listed.iter().zip(chosen.borrow().iter()).filter(|(_, on)| **on).map(|(b, _)| b.branch.clone()).collect();
                    if picked.is_empty() {
                        return;
                    }
                    window.close_dialog(cx);
                    let repo = repo.clone();
                    let _ = this.update(cx, |w, cx| w.delete_merged_branches(repo, picked, window, cx));
                }
            };
            dialog
                .title(format!("Remote branches of your merged PRs in {name}"))
                .w(px(640.))
                .content(move |content, _, cx| {
                    let muted = cx.theme().muted_foreground;
                    let mut list = v_flex().id("merged-branches").gap_1().max_h(px(380.)).overflow_y_scroll();
                    for (ix, b) in listed.iter().enumerate() {
                        let chosen = chosen.clone();
                        let on = chosen.borrow()[ix];
                        list = list.child(
                            v_flex()
                                .child(Checkbox::new(("merged-branch", ix)).label(b.branch.clone()).checked(on).on_click(move |on, window, _| {
                                    chosen.borrow_mut()[ix] = *on;
                                    window.refresh();
                                }))
                                .child(div().pl_6().text_xs().text_color(muted).truncate().child(format!("#{} {} · merged {}", b.number, b.title, b.merged))),
                        );
                    }
                    content.child(
                        v_flex()
                            .gap_3()
                            .child(list)
                            .child(div().text_xs().text_color(muted).child(
                                "Only the branches on origin go: each merged request stays on GitHub or GitLab with its commits and its review. Only your own PRs are listed: a colleague's branch is theirs to delete. Branches checked out by a task, with an open PR, or with commits since the merge are not listed either.",
                            )),
                    )
                })
                .footer({
                    let delete = delete.clone();
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("merged-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(
                            Button::new("merged-delete")
                                .danger()
                                .label(format!("Delete {count} branch{}", if count == 1 { "" } else { "es" }))
                                .disabled(count == 0)
                                .on_click(move |_, window, cx| delete(window, cx)),
                        )
                })
        });
    }

    /// Deletes `branches` from the project's origin, in the background,
    /// and tells what went and what did not.
    fn delete_merged_branches(&mut self, repo: PathBuf, branches: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pi) = self.projects.iter().position(|p| p.path == repo) else { return };
        // A task may have taken one of them since the list was made.
        let in_use = self.branches_in_use(pi);
        let branches: Vec<String> = branches.into_iter().filter(|b| !in_use.contains(b)).collect();
        if branches.is_empty() {
            return;
        }
        self.projects[pi].merged_branches.retain(|b| !branches.contains(&b.branch));
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let job = repo.clone();
            let done = cx.background_spawn(crate::procs::on_thread(move || merged_branches::delete(&job, &branches))).await;
            let _ = this.update_in(cx, |this, window, cx| {
                PROTECTED.with(|known| {
                    let mut known = known.borrow_mut();
                    for (branch, why) in &done.failed {
                        if why.starts_with("protected") {
                            known.insert((repo.clone(), branch.clone()));
                        }
                    }
                });
                if let Some(p) = this.projects.iter_mut().find(|p| p.path == repo) {
                    p.merged_checked = None;
                }
                this.refresh_merged_branches(window, cx);
                let n = done.deleted.len();
                let deleted = format!("Deleted {n} remote branch{}.", if n == 1 { "" } else { "es" });
                let note = if done.failed.is_empty() {
                    Notification::success(deleted)
                } else {
                    let failed = done.failed.iter().map(|(b, why)| format!("{b}: {why}")).collect::<Vec<_>>().join("\n");
                    Notification::warning(format!("{deleted}\nNot deleted:\n{failed}")).autohide(false)
                };
                window.push_notification(note.title("Merged branches"), cx);
            });
        })
        .detach();
    }
}
