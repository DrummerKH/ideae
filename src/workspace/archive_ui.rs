//! Archive, the everyday end of a task, and restore: the list of a
//! project's archived tasks, and the dialog that brings one back to go
//! on with its goal (archive.rs has what is in an archive and how).

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

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

use super::{TaskEntry, Workspace};
use crate::{
    agents,
    archive::{self, Applied, Archived, Conversation, PrNow, Start},
    git, naming, state,
};

/// What a task being archived shows on its row.
pub const ARCHIVING: &str = "archiving…";

/// "1.2 MB", "34 kB".
fn size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{} kB", bytes / 1_000),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

/// The archives a list shows, with what a search looks in: none while
/// they are read.
type Listed = Rc<RefCell<Option<Vec<(Archived, String)>>>>;

/// The deferred first, the soonest back first and those with no day after;
/// then the archived, newest first.
fn sort_listed(all: &mut [(Archived, String)]) {
    all.sort_by_key(|(a, _)| (a.deferred.is_none(), a.deferred.map(|d| d.until.unwrap_or(u64::MAX)), std::cmp::Reverse(a.meta.archived)));
}

/// When in the day a deferred task comes back: its name and the hour.
const DEFER_AT: [(&str, i32); 3] = [("Morning", 9), ("Afternoon", 14), ("Evening", 19)];

/// The days a task can be put off to, from `now`: a name and how many days
/// ahead, or none for a day not set. The weekend is the coming Saturday,
/// next week its Monday.
fn defer_days(now: u64) -> Vec<(&'static str, Option<i32>)> {
    let weekday = crate::local_time::weekday(now) as i32;
    let saturday = match (6 - weekday).rem_euclid(7) {
        0 => 7,
        n => n,
    };
    let monday = match (1 - weekday).rem_euclid(7) {
        0 => 7,
        n => n,
    };
    // On the weekend itself, "this weekend" is the next one.
    let weekend = if weekday == 6 || weekday == 0 { "Next weekend" } else { "This weekend" };
    vec![
        ("Tomorrow", Some(1)),
        (weekend, Some(saturday)),
        ("Next week", Some(monday)),
        ("In 2 weeks", Some(14)),
        ("In a month", Some(30)),
        ("In 3 months", Some(91)),
        ("In 6 months", Some(182)),
        ("Not sure yet", None),
    ]
}

/// Asks when a task comes back, a day and a time of it, and does `pick`
/// with the answer.
fn defer_dialog(title: String, told: String, window: &mut Window, cx: &mut App, pick: impl Fn(archive::Deferred, &mut Window, &mut App) + 'static) {
    let pick = Rc::new(pick);
    let hour = Rc::new(std::cell::Cell::new(DEFER_AT[0].1));
    window.open_dialog(cx, move |dialog, _, _| {
        let (told, pick, hour) = (told.clone(), pick.clone(), hour.clone());
        dialog.title(title.clone()).w(px(560.)).content(move |content, _, cx| {
            let now = state::now();
            let muted = cx.theme().muted_foreground;
            let at = hour.get();
            let mut times = h_flex().gap_1();
            for (ix, (label, h)) in DEFER_AT.into_iter().enumerate() {
                let hour = hour.clone();
                times = times.child(Button::new(("defer-at", ix)).small().ghost().label(label).selected(at == h).on_click(move |_, window, _| {
                    hour.set(h);
                    window.refresh();
                }));
            }
            let mut choices = div().grid().grid_cols(2).gap_1();
            for (ix, (label, days)) in defer_days(now).into_iter().enumerate() {
                let pick = pick.clone();
                let until = days.map(|d| crate::local_time::at(now, d, at));
                let when = until.map(|u| crate::local_time::day(u, now)).unwrap_or_default();
                choices = choices.child(
                    Button::new(("defer-for", ix))
                        .small()
                        .outline()
                        .w_full()
                        .child(h_flex().w_full().justify_between().gap_2().child(label).child(div().text_color(muted).child(when)))
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            pick(archive::Deferred { since: state::now(), until }, window, cx);
                        }),
                );
            }
            content.child(
                v_flex()
                    .gap_3()
                    .child(div().text_sm().text_color(muted).child(told.clone()))
                    .child(h_flex().gap_2().items_center().child(div().text_sm().child("At")).child(times))
                    .child(v_flex().gap_1().child(div().text_sm().child("Back")).child(choices)),
            )
        })
    });
}

/// What the restore dialog is set to.
#[derive(Clone)]
struct Choice {
    /// The PR as it is now: None while gh is asked, Some(None) when
    /// nothing could be told.
    pr: Option<Option<PrNow>>,
    on_branch: bool,
    commits: bool,
    changes: bool,
    summary: bool,
    /// The new branch's name, when it starts from origin; empty for one
    /// named from the title.
    branch: String,
}

impl Workspace {
    /// Archives a task: one click, nothing is lost. Only an agent in the
    /// middle of a turn is asked about first.
    pub fn archive_task(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        let t = &self.projects[pi].tasks[ti];
        if t.main || t.side.is_some() || t.busy.is_some() {
            return;
        }
        if t.view.as_ref().is_some_and(|v| v.read(cx).agent_busy()) {
            let (this, path) = (cx.weak_entity(), path.to_path_buf());
            window.open_alert_dialog(cx, move |dialog, _, _| {
                let (this, path) = (this.clone(), path.clone());
                dialog
                    .confirm()
                    .title("Archive while the agent works?")
                    .description("It is in the middle of a turn: it is stopped, and the conversation so far goes into the archive.")
                    .ok_text("Archive")
                    .on_ok(move |_, window, cx| {
                        let _ = this.update(cx, |this, cx| this.archive_now(&path, None, window, cx));
                        true
                    })
            });
            return;
        }
        self.archive_now(path, None, window, cx);
    }

    /// Defers a task: archived as Archive does, kept from the clean-up, and
    /// back as a paused task on the day chosen here, if one is.
    pub fn defer_task(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        let t = &self.projects[pi].tasks[ti];
        if t.main || t.side.is_some() || t.busy.is_some() {
            return;
        }
        let busy = t.view.as_ref().is_some_and(|v| v.read(cx).agent_busy());
        let mut told = "Its worktree goes, as with Archive; its goal and its agent's conversation are kept and not cleaned up. On the day, it comes back paused, from the current origin or its open PR's branch, its agent told what changed when you open it.".to_string();
        if busy {
            told.push_str(" Its agent is in the middle of a turn: it is stopped.");
        }
        let (this, path) = (cx.weak_entity(), path.to_path_buf());
        let title = format!("Defer \"{}\"", t.label());
        defer_dialog(title, told, window, cx, move |deferred, window, cx| {
            let _ = this.update(cx, |w, cx| w.archive_now(&path, Some(deferred), window, cx));
        });
    }

    /// Defers an archived task, changes the day of a deferred one, or with
    /// None makes it an archive like the others; the list is told.
    fn set_deferred(&mut self, file: PathBuf, deferred: Option<archive::Deferred>, listed: Listed, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.archive_dir.clone();
        cx.spawn_in(window, async move |_, cx| {
            let job = file.clone();
            let done = cx.background_spawn(async move { archive::set_deferred(&root, &job, deferred) }).await;
            let _ = cx.update(|window, cx| match done {
                Ok(()) => {
                    if let Some(all) = listed.borrow_mut().as_mut() {
                        for (item, _) in all.iter_mut().filter(|(item, _)| item.file == file) {
                            item.deferred = deferred;
                        }
                        sort_listed(all);
                    }
                    window.refresh();
                }
                Err(e) => window.push_notification(Notification::error(e).title("Could not change the archive"), cx),
            });
        })
        .detach();
    }

    /// Stops the task as Pause does, writes its archive (deferred, when it
    /// is), and only once that is read back whole removes its worktree and
    /// local branch.
    pub(super) fn archive_now(&mut self, path: &Path, deferred: Option<archive::Deferred>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        let t = &self.projects[pi].tasks[ti];
        if t.main || t.side.is_some() || t.busy.is_some() {
            return;
        }
        let said = t.view.as_ref().and_then(|v| v.read(cx).agent_said.as_ref().map(|s| s.whole.to_string()));
        self.stop(path, window, cx);
        let Some((pi, ti)) = self.find(path) else { return };
        let repo = self.projects[pi].path.clone();
        let settings = state::settings();
        let archive_dir = self.archive_dir.clone();
        let t = &mut self.projects[pi].tasks[ti];
        t.busy = Some(ARCHIVING.into());
        let profiles: Vec<Option<agents::Profile>> = t.sessions.iter().map(|s| settings.profile(&s.profile).cloned()).collect();
        let sessions = t
            .sessions
            .iter()
            .zip(&profiles)
            .map(|(s, p)| archive::SessionMeta {
                profile: s.profile.clone(),
                name: p.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
                kind: p.as_ref().map(|p| p.kind).unwrap_or_default(),
                id: s.id.clone(),
                started: s.started,
                files: Vec::new(),
            })
            .collect();
        let meta = archive::Meta {
            title: t.label(),
            prompt: t.prompt.clone(),
            project: repo.clone(),
            worktree: path.to_path_buf(),
            branch: t.branch.clone(),
            base: t.base.clone(),
            pr: t.pr.as_ref().map(|pr| archive::PrMeta { number: pr.number, url: pr.url.clone(), state: pr.state.word().into() }),
            review: t.session.review.clone(),
            said,
            sessions,
            agent: t.agent.clone(),
            note: t.note.clone(),
            created: t.created,
            ..Default::default()
        };
        // Archived again after a restore: in the place of the one it came from.
        let key = if !t.archive.is_empty() && !t.archive_keep { t.archive.clone() } else { archive::new_key(&t.branch, state::now()) };
        let dest = archive::file_of(&archive_dir, &repo, &key);
        let (branch, unlock, title) = (t.branch.clone(), t.locked.is_some(), t.label());
        cx.notify();
        let torn_down = self.torn_down(path);
        let path = path.to_path_buf();
        cx.spawn_in(window, async move |this, cx| {
            torn_down.await;
            let (root, file) = (archive_dir.clone(), dest.clone());
            let job = archive::Job { meta, profiles, dest };
            let task_path = path.clone();
            // Written: whether the archive is there, whatever came after.
            let result = cx
                .background_spawn(async move {
                    archive::write(job).map_err(|e| (false, e))?;
                    // Not deferred, an archive of the same key is not either.
                    archive::set_deferred(&root, &file, deferred).map_err(|e| (false, e))?;
                    git::remove_task_worktree(&repo, &task_path, &branch, unlock).map_err(|e| (true, e))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(stays) => {
                        if let Some((pi, ti)) = this.find(&path) {
                            this.projects[pi].tasks.remove(ti);
                        }
                        if let Some(stays) = stays {
                            window.push_notification(Notification::warning(stays), cx);
                        }
                        if let Some(left) = this.dissolve_lonely_groups() {
                            this.switch(&left, window, cx);
                        }
                        let told = match deferred {
                            Some(d) if d.until.is_some() => format!("\"{title}\" comes back {} as a paused task; till then it is with the archived ones, kept.", d.told().trim_start_matches("deferred ")),
                            Some(_) => format!("\"{title}\" is kept with the archived ones, not cleaned up, till you restore it or set a day."),
                            None => format!("\"{title}\" is archived with its agent's conversation: the project's + brings it back."),
                        };
                        window.push_notification(Notification::info(told).title(if deferred.is_some() { "Deferred" } else { "Archived" }), cx);
                        this.save(cx);
                    }
                    Err((written, e)) => {
                        if let Some(entry) = this.entry_mut(&path) {
                            entry.busy = None;
                            if written {
                                entry.archive = key.clone();
                                entry.archive_keep = false;
                            }
                        }
                        let (title, told) = if written {
                            (if deferred.is_some() { "Deferred, but the worktree stays" } else { "Archived, but the worktree stays" }, format!("The archive is written; removing the worktree failed: {e}"))
                        } else {
                            (if deferred.is_some() { "Defer failed" } else { "Archive failed" }, format!("Nothing was removed. {e}"))
                        };
                        window.push_notification(Notification::error(told).title(title), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Archives every task of the project whose PR was merged, after a list.
    pub fn archive_merged(&mut self, project: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(project) else { return };
        let merged: Vec<(PathBuf, String)> = p
            .tasks
            .iter()
            .filter(|t| !t.main && t.side.is_none() && t.busy.is_none() && t.pr.as_ref().is_some_and(|pr| pr.state == crate::pr::PrState::Merged))
            .map(|t| (t.path.clone(), t.label()))
            .collect();
        if merged.is_empty() {
            window.push_notification(Notification::info("No task of this project has a merged PR."), cx);
            return;
        }
        let names = merged.iter().map(|(_, l)| format!("• {l}")).collect::<Vec<_>>().join("\n");
        let description = format!("Each goes into an archive with its agent's conversation; their worktrees and local branches go.\n{names}");
        let this = cx.weak_entity();
        let paths: Vec<PathBuf> = merged.into_iter().map(|(p, _)| p).collect();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, paths) = (this.clone(), paths.clone());
            dialog
                .confirm()
                .title(format!("Archive {} merged task{}?", paths.len(), if paths.len() == 1 { "" } else { "s" }))
                .description(description.clone())
                .ok_text("Archive")
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |w, cx| {
                        for path in &paths {
                            w.archive_now(path, None, window, cx);
                        }
                    });
                    true
                })
        });
    }

    /// Deletes the archives older than Settings say, in the background.
    pub(super) fn clean_up_archives(&mut self, cx: &mut Context<Self>) {
        // Java's index of a worktree that is gone, with the archives' sweep.
        cx.background_spawn(async move { crate::lsp::prune_jdtls() }).detach();
        let days = state::with_settings(|s| s.archive_keep_days);
        if days == 0 {
            return;
        }
        let root = self.archive_dir.clone();
        cx.background_spawn(async move { archive::clean_up(&root, u64::from(days) * 86_400, state::now()) }).detach();
    }

    /// The archived tasks of a project, or of every project in the window:
    /// searched, restored, deleted for good. None while presenting: they
    /// are not among the tasks chosen.
    pub fn archived_dialog(&mut self, project: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let projects: Vec<PathBuf> = match project {
            _ if self.is_presenting() => Vec::new(),
            Some(pi) => self.projects.get(pi).map(|p| p.path.clone()).into_iter().collect(),
            None => self.projects.iter().enumerate().filter(|(pi, _)| self.in_space(*pi)).map(|(_, p)| p.path.clone()).collect(),
        };
        let names: std::collections::HashMap<PathBuf, String> = self.projects.iter().map(|p| (p.path.clone(), p.name.clone())).collect();
        let title = match project.and_then(|pi| self.projects.get(pi)) {
            Some(p) => format!("Archived tasks of {}", p.name),
            None => "Archived tasks".to_string(),
        };
        let presenting = self.is_presenting();
        // None while they are read; each with what a search looks in.
        let listed: Listed = Rc::default();
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search: title, prompt, branch, PR, date"));
        let root = self.archive_dir.clone();
        {
            let listed = listed.clone();
            cx.spawn_in(window, async move |_, cx| {
                let found = cx
                    .background_spawn(async move {
                        archive::list(&root).into_iter().filter(|a| projects.contains(&a.meta.project)).map(|a| {
                            let hay = a.haystack();
                            (a, hay)
                        }).collect::<Vec<_>>()
                    })
                    .await;
                let mut found = found;
                sort_listed(&mut found);
                *listed.borrow_mut() = Some(found);
                let _ = cx.update(|window, _| window.refresh());
            })
            .detach();
        }
        let refresh = cx.subscribe_in(&query, window, |_, _, event: &InputEvent, window, _| {
            if matches!(event, InputEvent::Change) {
                window.refresh();
            }
        });
        let listening = Rc::new(refresh);
        let this = cx.weak_entity();
        let focused = Rc::new(std::cell::Cell::new(false));
        let several = project.is_none();
        window.open_dialog(cx, move |dialog, _, _| {
            let (listed, query, this, focused, names, _keep) = (listed.clone(), query.clone(), this.clone(), focused.clone(), names.clone(), listening.clone());
            // Enter in the search reaches the dialog: the list stays.
            dialog.title(title.clone()).w(px(720.)).on_ok(|_, _, _| false).content(move |content, window, cx| {
                if !focused.replace(true) {
                    let query = query.clone();
                    window.defer(cx, move |window, cx| query.read(cx).focus_handle(cx).focus(window, cx));
                }
                let muted = cx.theme().muted_foreground;
                let words: Vec<String> = query.read(cx).value().to_lowercase().split_whitespace().map(str::to_string).collect();
                let mut list = v_flex().id("archived-list").gap_0p5().max_h(px(460.)).overflow_y_scroll();
                let shown = listed.borrow();
                match shown.as_ref() {
                    _ if presenting => list = list.child(div().text_sm().text_color(muted).child("Not shown while presenting.")),
                    None => list = list.child(div().text_sm().text_color(muted).child("Reading…")),
                    Some(all) if all.is_empty() => list = list.child(div().text_sm().text_color(muted).child("No task archived yet. A task's menu has Archive: its worktree goes, its goal and its agent's conversation stay here. Defer… keeps it from the clean-up and brings it back on a day.")),
                    Some(all) => {
                        let mut any = false;
                        // Archives whose task is restored and here: its path, and whether the
                        // archive keeps work that did not come back.
                        let live: std::collections::HashMap<String, (PathBuf, bool)> = this
                            .upgrade()
                            .map(|w| w.read(cx).projects.iter().flat_map(|p| &p.tasks).filter(|t| !t.archive.is_empty()).map(|t| (t.archive.clone(), (t.path.clone(), t.archive_keep))).collect())
                            .unwrap_or_default();
                        // Deferred and archived apart, when there are both.
                        let both = all.iter().any(|(a, _)| a.deferred.is_some()) && all.iter().any(|(a, _)| a.deferred.is_none());
                        let mut heading: Option<bool> = None;
                        for (ix, (item, hay)) in all.iter().enumerate() {
                            let deferred = item.deferred;
                            if !words.iter().all(|w| hay.contains(w) || (deferred.is_some() && "deferred".contains(w.as_str()))) {
                                continue;
                            }
                            any = true;
                            if both && heading != Some(deferred.is_some()) {
                                heading = Some(deferred.is_some());
                                let label = if deferred.is_some() { "Deferred: kept, back on their day" } else { "Archived" };
                                list = list.child(div().pt_1().px_2().text_xs().text_color(muted).child(label));
                            }
                            let m = &item.meta;
                            let mut details = Vec::new();
                            if several {
                                details.push(names.get(&m.project).cloned().unwrap_or_default());
                            }
                            details.push(m.branch.clone());
                            if let Some(pr) = &m.pr {
                                details.push(format!("PR #{} {}", pr.number, pr.state));
                            }
                            match deferred {
                                Some(d) => details.push(d.told()),
                                None => details.push(format!("archived {}", archive::date(m.archived))),
                            }
                            if !m.unpushed.is_empty() || !m.uncommitted.is_empty() {
                                details.push("with work on no remote".into());
                            }
                            details.push(size(item.size));
                            let live = live.get(&item.key()).cloned();
                            if let Some((_, keep)) = &live {
                                details.insert(0, if *keep { "restored, in the list; this keeps what did not come back".into() } else { "restored, in the list".into() });
                            }
                            let prompt = m.prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().to_string();
                            let (restore_item, delete_item, row_item) = (item.clone(), item.clone(), item.clone());
                            let (a, b, c, listed) = (this.clone(), this.clone(), this.clone(), listed.clone());
                            // Deferred: another day, or an archive like the others; archived: deferred.
                            let (day, undefer) = (this.clone(), this.clone());
                            let (day_file, undefer_file, day_listed, undefer_listed) = (item.file.clone(), item.file.clone(), listed.clone(), listed.clone());
                            let day_title = m.title.clone();
                            list = list.child(
                                h_flex()
                                    .id(("archived", ix))
                                    .gap_2()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(cx.theme().accent))
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .child(div().text_sm().truncate().child(m.title.clone()))
                                            .child(div().text_xs().text_color(muted).truncate().child(details.join(" · ")))
                                            .when(!prompt.is_empty() && prompt.trim() != m.title.trim(), |col| col.child(div().text_xs().text_color(muted).truncate().child(prompt))),
                                    )
                                    .map(|row| match live.clone() {
                                        // Restored twice, two tasks would write one archive.
                                        Some((path, _)) => row.child(Button::new(("archived-open", ix)).small().primary().label("Open").tooltip("Restored already: go to the task").on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            window.close_dialog(cx);
                                            let _ = a.update(cx, |w, cx| w.switch(&path, window, cx));
                                        })),
                                        None => row.child(Button::new(("archived-restore", ix)).small().primary().label("Restore…").on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            window.close_dialog(cx);
                                            let item = restore_item.clone();
                                            let _ = a.update(cx, |w, cx| w.restore_dialog(item, window, cx));
                                        })),
                                    })
                                    .when(live.is_none(), |row| row.child(Button::new(("archived-defer", ix)).small().ghost().label(if deferred.is_some() { "Day…" } else { "Defer…" }).on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        let (this, file, listed) = (day.clone(), day_file.clone(), day_listed.clone());
                                        let told = "It stays with the archived tasks, kept from the clean-up; on the day it comes back paused, from the current origin or its open PR's branch.".to_string();
                                        defer_dialog(format!("Defer \"{day_title}\""), told, window, cx, move |d, window, cx| {
                                            let _ = this.update(cx, |w, cx| w.set_deferred(file.clone(), Some(d), listed.clone(), window, cx));
                                        });
                                    })))
                                    .when(deferred.is_some(), |row| {
                                        row.child(Button::new(("archived-undefer", ix)).small().ghost().label("Archive").tooltip("An archive like the others: no day, cleaned up in time").on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            let _ = undefer.update(cx, |w, cx| w.set_deferred(undefer_file.clone(), None, undefer_listed.clone(), window, cx));
                                        }))
                                    })
                                    .child(Button::new(("archived-delete", ix)).small().ghost().label("Delete…").on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        let item = delete_item.clone();
                                        let _ = b.update(cx, |w, cx| w.delete_archived_dialog(item, listed.clone(), window, cx));
                                    }))
                                    .on_click(move |_, window, cx| {
                                        window.close_dialog(cx);
                                        let item = row_item.clone();
                                        let _ = c.update(cx, |w, cx| match &live {
                                            Some((path, _)) => w.switch(path, window, cx),
                                            None => w.restore_dialog(item, window, cx),
                                        });
                                    }),
                            );
                        }
                        if !any {
                            list = list.child(div().text_sm().text_color(muted).child("None matches."));
                        }
                    }
                }
                content.child(v_flex().gap_2().child(Input::new(&query)).child(list))
            })
        });
    }

    /// Deletes an archive for good, after saying what goes with it.
    fn delete_archived_dialog(&mut self, item: Archived, listed: Listed, window: &mut Window, cx: &mut Context<Self>) {
        let m = item.meta.clone();
        let mut description = "Its agent's conversation goes with it.".to_string();
        if !m.unpushed.is_empty() {
            description.push_str(&format!("\n{} commit(s) that were on no remote are lost.", m.unpushed.len()));
        }
        if !m.uncommitted.is_empty() {
            description.push_str(&format!("\nUncommitted changes to {} file(s) are lost.", m.uncommitted.len()));
        }
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (file, listed) = (item.file.clone(), listed.clone());
            dialog
                .confirm()
                .title(format!("Delete the archive of \"{}\"?", m.title))
                .description(description.clone())
                .ok_text("Delete")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let (file, listed) = (file.clone(), listed.clone());
                    window
                        .spawn(cx, async move |cx| {
                            let job = file.clone();
                            let gone = cx.background_spawn(async move { std::fs::remove_file(&job) }).await;
                            let _ = cx.update(|window, cx| {
                                match gone {
                                    Ok(()) => {
                                        if let Some(all) = listed.borrow_mut().as_mut() {
                                            all.retain(|(a, _)| a.file != file);
                                        }
                                        window.refresh();
                                    }
                                    Err(e) => window.push_notification(Notification::error(e.to_string()).title("Could not delete the archive"), cx),
                                }
                            });
                        })
                        .detach();
                    true
                })
        });
    }

    /// How to bring an archived task back: where from, its work, its
    /// conversation. Its PR is asked about meanwhile.
    pub fn restore_dialog(&mut self, item: Archived, window: &mut Window, cx: &mut Context<Self>) {
        // Restored already: a second task of it would write the same archive.
        let key = item.key();
        if let Some(path) = self.projects.iter().flat_map(|p| &p.tasks).find(|t| t.archive == key).map(|t| t.path.clone()) {
            return self.switch(&path, window, cx);
        }
        let Some(pi) = self.projects.iter().position(|p| p.path == item.meta.project) else {
            let told = format!("Its project, {}, is not in the list: add it first.", item.meta.project.display());
            window.push_notification(Notification::error(told).title("Restore"), cx);
            return;
        };
        let repo = self.projects[pi].path.clone();
        let m = item.meta.clone();
        let summary = state::with_settings(|s| s.restore_summary);
        let choice = Rc::new(RefCell::new(Choice { pr: if m.pr.is_some() { None } else { Some(None) }, on_branch: false, commits: true, changes: true, summary, branch: String::new() }));
        let branch_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_value(naming::branch_name(&m.title), window, cx);
            input
        });
        let typed = cx.subscribe_in(&branch_input, window, |_, _, event: &InputEvent, window, _| {
            if matches!(event, InputEvent::Change) {
                window.refresh();
            }
        });
        let typed = Rc::new(typed);
        if let Some(number) = m.pr.as_ref().map(|p| p.number) {
            let choice = choice.clone();
            cx.spawn_in(window, async move |_, cx| {
                let now = cx.background_spawn(async move { crate::pr::by_number(&repo, number) }).await;
                let now = now.map(|(pr, head)| PrNow { state: pr.state, head });
                let mut c = choice.borrow_mut();
                c.on_branch = now.as_ref().is_some_and(|p| matches!(p.state, crate::pr::PrState::Open | crate::pr::PrState::Draft) && !p.head.is_empty());
                c.pr = Some(now);
                drop(c);
                let _ = cx.update(|window, _| window.refresh());
            })
            .detach();
        }
        let profiles = state::with_settings(|s| s.agents.clone());
        let fallback = self.project_profile(pi);
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let (choice, m, this, item, branch_input, _typed) = (choice.clone(), m.clone(), this.clone(), item.clone(), branch_input.clone(), typed.clone());
            let muted = cx.theme().muted_foreground;
            let c = choice.borrow().clone();
            let pr_now = c.pr.clone().flatten();
            let offer = archive::offer(&m, pr_now.as_ref(), &profiles, fallback.as_ref(), c.summary);
            let resumable = !matches!(archive::offer(&m, pr_now.as_ref(), &profiles, fallback.as_ref(), false).conversation, Conversation::Summary { .. });
            let pr_line = match (&m.pr, &c.pr) {
                (None, _) => "It had no pull request.".to_string(),
                (Some(p), None) => format!("PR #{}: asking GitHub…", p.number),
                (Some(p), Some(Some(now))) => format!("PR #{}: {}", p.number, now.state.word()),
                (Some(p), Some(None)) => format!("PR #{}: {} when archived; gh could not tell now", p.number, p.state),
            };
            let pr_branch = offer.pr_branch.clone();
            let set = |change: fn(&mut Choice, bool)| {
                let choice = choice.clone();
                move |on: bool, window: &mut Window| {
                    change(&mut choice.borrow_mut(), on);
                    window.refresh();
                }
            };
            let (to_origin, to_branch) = (set(|c, _| c.on_branch = false), set(|c, _| c.on_branch = true));
            let (full, short) = (set(|c, _| c.summary = false), set(|c, _| c.summary = true));
            let (commits, changes) = (set(|c, on| c.commits = on), set(|c, on| c.changes = on));
            let conversation = match &offer.conversation {
                Conversation::Resume { profile, id, .. } => format!("{} resumes session {}: the whole conversation, then a short note of what changed since.", profile.name, archive::prefix(id, 8)),
                Conversation::Summary { profile: Some(p), why } => format!("{} starts a new session, told the task, what was said last and what changed since ({why}).", p.name),
                Conversation::Summary { profile: None, .. } => "No agent is set up: the task opens with a shell.".to_string(),
            };
            let on_branch = c.on_branch && pr_branch.is_some();
            let new_branch = branch_input.read(cx).value().trim().to_string();
            let problem = if on_branch { None } else { naming::branch_problem(&new_branch) };
            let restore = {
                let (choice, item, new_branch) = (choice.clone(), item.clone(), new_branch.clone());
                move |window: &mut Window, cx: &mut App| {
                    if problem.is_some() {
                        return;
                    }
                    let mut c = choice.borrow().clone();
                    c.branch = new_branch.clone();
                    window.close_dialog(cx);
                    let _ = this.update(cx, |w, cx| w.restore_archived(item.clone(), c, false, window, cx));
                }
            };
            dialog
                .title(format!("Restore \"{}\"", m.title))
                .w(px(620.))
                // Enter in the branch name reaches the dialog: it restores, as the button does.
                .on_ok({
                    let restore = restore.clone();
                    move |_, window, cx| {
                        restore(window, cx);
                        false
                    }
                })
                .content(move |content, _, cx| {
                    let mut form = v_flex()
                        .gap_3()
                        .child(div().text_sm().child(format!("Archived {} from the branch {}. {pr_line}", archive::date(m.archived), m.branch)))
                        .child(
                            v_flex()
                                .gap_1()
                                .child(div().text_sm().child("Start from"))
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child({
                                            let to_origin = to_origin.clone();
                                            Button::new("restore-origin").small().outline().label("A new branch from origin").selected(!on_branch).on_click(move |_, window, _| to_origin(false, window))
                                        })
                                        .when_some(pr_branch.clone(), |row, head| {
                                            let to_branch = to_branch.clone();
                                            row.child(Button::new("restore-branch").small().outline().label(format!("Continue on its branch {head}")).selected(on_branch).on_click(move |_, window, _| to_branch(true, window)))
                                        }),
                                )
                                .child(div().text_xs().text_color(muted).child(if on_branch {
                                    "Its PR is still open: its work is not in main yet, so the PR's branch is fetched and checked out."
                                } else {
                                    "As a new task is made: the current origin, on a new branch."
                                })),
                        );
                    if !on_branch {
                        let danger = cx.theme().danger;
                        form = form.child(
                            v_flex()
                                .gap_1()
                                .child(div().text_sm().child("Branch"))
                                .child(Input::new(&branch_input).small())
                                .child(match problem {
                                    Some(problem) => div().text_xs().text_color(danger).child(problem),
                                    None => div().text_xs().text_color(muted).child("Named from the title, as a new task's; a name that is taken gets a number."),
                                }),
                        );
                    }
                    if offer.commits > 0 || offer.changes > 0 {
                        let mut work = v_flex().gap_1().child(div().text_sm().child("Work that was on no remote"));
                        if offer.commits > 0 {
                            let commits = commits.clone();
                            work = work.child(Checkbox::new("restore-commits").label(format!("Bring back {} commit{} (cherry-picked)", offer.commits, if offer.commits == 1 { "" } else { "s" })).checked(c.commits).on_click(move |on, window, _| commits(*on, window)));
                        }
                        if offer.changes > 0 {
                            let changes = changes.clone();
                            work = work.child(Checkbox::new("restore-changes").label(format!("Bring back uncommitted changes to {} file{}", offer.changes, if offer.changes == 1 { "" } else { "s" })).checked(c.changes).on_click(move |on, window, _| changes(*on, window)));
                        }
                        form = form.child(work.child(div().text_xs().text_color(muted).child("What does not apply cleanly is kept and the agent is told where; the archive keeps it too.")));
                    }
                    let mut agent = v_flex().gap_1().child(div().text_sm().child("The agent"));
                    if resumable {
                        agent = agent.child(
                            h_flex()
                                .gap_1()
                                .child({
                                    let full = full.clone();
                                    Button::new("restore-full").small().outline().label("The full conversation").selected(!c.summary).on_click(move |_, window, _| full(false, window))
                                })
                                .child({
                                    let short = short.clone();
                                    Button::new("restore-summary").small().outline().label("A summary only").selected(c.summary).on_click(move |_, window, _| short(true, window))
                                }),
                        );
                    }
                    form = form.child(agent.child(div().text_xs().text_color(muted).child(conversation.clone())));
                    content.child(form)
                })
                .footer({
                    let restore = restore.clone();
                    h_flex().justify_end().child(Button::new("restore-start").primary().label("Restore").disabled(problem.is_some()).on_click(move |_, window, cx| restore(window, cx)))
                })
        });
    }

    /// The agent a project's new tasks run, as a profile.
    fn project_profile(&self, pi: usize) -> Option<agents::Profile> {
        let id = self.project_agent(pi);
        state::with_settings(|s| s.profile(&id).cloned())
    }

    /// Deferred tasks whose day came are restored as Restore would, from
    /// origin or their open PR's branch, and left paused: nothing starts
    /// till they are opened. Not while presenting; a task restored from
    /// the same archive already is not made twice.
    pub(super) fn wake_deferred(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_presenting() {
            return;
        }
        let projects: Vec<PathBuf> = self.projects.iter().map(|p| p.path.clone()).collect();
        let root = self.archive_dir.clone();
        cx.spawn_in(window, async move |this, cx| {
            let now = state::now();
            let due: Vec<(Archived, Option<PrNow>)> = cx
                .background_spawn(async move {
                    archive::list(&root)
                        .into_iter()
                        .filter(|a| a.deferred.is_some_and(|d| d.due(now)) && projects.contains(&a.meta.project))
                        .map(|a| {
                            let pr = a.meta.pr.as_ref().and_then(|pr| crate::pr::by_number(&a.meta.project, pr.number)).map(|(pr, head)| PrNow { state: pr.state, head });
                            (a, pr)
                        })
                        .collect()
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                for (item, pr) in due {
                    if this.waking.contains(&item.file) || this.wake_failed.contains(&item.file) {
                        continue;
                    }
                    let key = item.key();
                    if this.projects.iter().flat_map(|p| &p.tasks).any(|t| t.archive == key) {
                        // Restored by hand already: only its day goes.
                        let (root, file) = (this.archive_dir.clone(), item.file.clone());
                        cx.background_spawn(async move { archive::set_deferred(&root, &file, None) }).detach();
                        continue;
                    }
                    let on_branch = pr.as_ref().is_some_and(|p| matches!(p.state, crate::pr::PrState::Open | crate::pr::PrState::Draft) && !p.head.is_empty());
                    let summary = state::with_settings(|s| s.restore_summary);
                    let choice = Choice { pr: Some(pr), on_branch, commits: true, changes: true, summary, branch: String::new() };
                    this.waking.insert(item.file.clone());
                    this.restore_archived(item, choice, true, window, cx);
                }
            });
        })
        .detach();
    }

    /// Brings an archived task back as a new task: its worktree made and
    /// its work and sessions put back in the background, then its agent
    /// started on the same conversation (or a summary) and told what
    /// changed since. A deferred one come back on its own (`woke`) is left
    /// paused, out of view: that is done when it is opened.
    fn restore_archived(&mut self, item: Archived, choice: Choice, woke: bool, window: &mut Window, cx: &mut Context<Self>) {
        let m = item.meta.clone();
        let Some(pi) = self.projects.iter().position(|p| p.path == m.project) else { return };
        let repo = self.projects[pi].path.clone();
        let settings = state::settings();
        let pr_now = choice.pr.clone().flatten();
        let fallback = self.project_profile(pi);
        let offer = archive::offer(&m, pr_now.as_ref(), &settings.agents, fallback.as_ref(), choice.summary);
        let start = match (&offer.pr_branch, choice.on_branch) {
            (Some(head), true) => Start::PrBranch(head.clone()),
            _ => Start::Origin,
        };
        // Its sessions' files go to profiles of the same kind only.
        let profiles: Vec<Option<agents::Profile>> = m.sessions.iter().map(|s| settings.profile(&s.profile).filter(|p| p.kind == s.kind).cloned()).collect();
        let wanted = match &start {
            Start::Origin if choice.branch.trim().is_empty() => naming::branch_name(&m.title),
            Start::Origin => choice.branch.trim().to_string(),
            Start::PrBranch(head) => head.clone(),
        };
        // Where it shows till its worktree is there: no task's path.
        let placeholder = git::task_worktree_dir(&repo, &format!("{wanted}.restoring-{}", state::now()));
        let p = &mut self.projects[pi];
        if !woke {
            p.collapsed = false;
        }
        p.tasks.push(TaskEntry { path: placeholder.clone(), branch: wanted.clone(), title: m.title.clone(), last_active: state::now(), busy: Some("restoring…".into()), created: state::now(), ..Default::default() });
        cx.notify();
        let file = item.file.clone();
        let key = item.key();
        let (root, was_deferred) = (self.archive_dir.clone(), item.deferred.is_some());
        let (copy_env, job_meta) = (settings.copy_env, m.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<archive::Restored, String> = cx
                .background_spawn(async move {
                    let branch = if matches!(start, Start::Origin) { git::free_branch_name(&repo, &wanted) } else { wanted };
                    let plan = archive::Plan { start, branch, commits: choice.commits, changes: choice.changes, profiles, copy_env };
                    let restored = archive::restore(&repo, &file, &job_meta, &plan)?;
                    // Back: it does not come back again; an archive like the others.
                    if was_deferred {
                        let _ = archive::set_deferred(&root, &file, None);
                    }
                    Ok(restored)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let restored = match result {
                    Ok(restored) => restored,
                    Err(e) => {
                        if let Some((pi, ti)) = this.find(&placeholder) {
                            this.projects[pi].tasks.remove(ti);
                        }
                        if woke {
                            this.waking.remove(&item.file);
                            this.wake_failed.insert(item.file.clone());
                            let told = format!("\"{}\" stays deferred: {e}\nRestore it from the archived tasks when that is mended.", m.title);
                            window.push_notification(Notification::error(told).title("A deferred task could not come back"), cx);
                        } else {
                            window.push_notification(Notification::error(e).title("Could not restore the task"), cx);
                        }
                        return cx.notify();
                    }
                };
                this.waking.remove(&item.file);
                let Some(entry) = this.entry_mut(&placeholder) else { return };
                let path = restored.path.clone();
                entry.path = path.clone();
                entry.branch = restored.branch.clone();
                entry.busy = None;
                entry.prompt = m.prompt.clone();
                entry.agent = m.agent.clone();
                entry.base = restored.base.clone();
                entry.archive = key.clone();
                entry.archive_keep = restored.left_something();
                entry.sessions = m.sessions.iter().map(|s| state::AgentSession { profile: s.profile.clone(), id: s.id.clone(), started: s.started }).collect();
                let text = match &offer.conversation {
                    Conversation::Resume { session, .. } => {
                        // The one resumed is the one it runs: the last.
                        let resumed = entry.sessions.remove(*session);
                        entry.sessions.push(resumed);
                        Some(archive::note(&m, pr_now.as_ref(), &restored))
                    }
                    Conversation::Summary { profile: Some(profile), .. } => {
                        let id = profile.adapter().chooses_session_id().then(agents::uuid);
                        entry.sessions.push(state::AgentSession { profile: profile.id.clone(), id, started: state::now() });
                        entry.fresh = true;
                        Some(archive::summary(&m, pr_now.as_ref(), &restored))
                    }
                    Conversation::Summary { profile: None, .. } => None,
                };
                let mut problems = Vec::new();
                if woke {
                    entry.woke = Some(text.unwrap_or_default());
                } else {
                    this.switch(&path, window, cx);
                    this.task_made(&path, window, cx);
                    let view = this.entry_mut(&path).and_then(|entry| entry.view.clone());
                    if let (Some(text), Some(view)) = (text, view) {
                        view.update(cx, |t, cx| t.queue_prompt(text, true, window, cx));
                    }
                }
                for (what, applied) in [("The commits on no remote", &restored.commits), ("The uncommitted changes", &restored.changes)] {
                    if let Applied::Failed { why, kept } = applied {
                        problems.push(format!("{what} did not apply ({why}); kept at {kept}."));
                    }
                }
                if woke && problems.is_empty() {
                    let told = format!("\"{}\" is back on {}, paused: its agent is told what changed when you open it.", m.title, restored.branch);
                    window.push_notification(Notification::info(told).title("A deferred task is back"), cx);
                } else if woke {
                    problems.push("The archive keeps them too.".into());
                    window.push_notification(Notification::warning(problems.join("\n")).title(format!("\"{}\" is back, not all of its work", m.title)), cx);
                } else if problems.is_empty() {
                    window.push_notification(Notification::info(format!("\"{}\" is back on {}.", m.title, restored.branch)).title("Restored"), cx);
                } else {
                    problems.push("The archive keeps them too.".into());
                    window.push_notification(Notification::warning(problems.join("\n")).title("Restored, not all of its work"), cx);
                }
                this.save(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Removes a worktree an agent made for itself; its branch goes when
    /// nothing on it would be lost. Asks first when it has uncommitted changes.
    pub fn remove_side(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        let t = &self.projects[pi].tasks[ti];
        let Some(side) = t.side.as_ref().filter(|_| t.busy.is_none()) else { return };
        if !side.dirty {
            return self.remove_side_now(path, window, cx);
        }
        let (this, path) = (cx.weak_entity(), path.to_path_buf());
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, path) = (this.clone(), path.clone());
            dialog
                .confirm()
                .title("Remove the agent's worktree?")
                .description("It has changes that are not committed: they are lost. Its commits stay on its branch unless they are on a remote or another branch already.")
                .ok_text("Remove")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.remove_side_now(&path, window, cx));
                    true
                })
        });
    }

    fn remove_side_now(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.stop(path, window, cx);
        let Some((pi, ti)) = self.find(path) else { return };
        let repo = self.projects[pi].path.clone();
        let entry = &mut self.projects[pi].tasks[ti];
        entry.busy = Some("removing…".into());
        let (path, branch) = (path.to_path_buf(), entry.branch.clone());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let job_path = path.clone();
            let result = cx.background_spawn(async move { git::remove_side_worktree(&repo, &job_path, &branch) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(kept) => {
                        if let Some((pi, ti)) = this.find(&path) {
                            this.projects[pi].tasks.remove(ti);
                        }
                        if let Some(kept) = kept {
                            window.push_notification(Notification::info(kept).title("Worktree removed"), cx);
                        }
                    }
                    Err(e) => {
                        if let Some(entry) = this.entry_mut(&path) {
                            entry.busy = None;
                        }
                        window.push_notification(Notification::error(e).title("Could not remove the worktree"), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(all(test, unix))]
mod tests {
    /// The weekend is the coming Saturday and next week its Monday, from any day.
    #[test]
    fn deferring_offers_the_coming_weekend_and_monday() {
        let base = crate::local_time::at(1_795_000_000, 0, 12);
        for ahead in 0..7 {
            let now = base + ahead * 86_400;
            let weekday = crate::local_time::weekday(now) as i32;
            let days = super::defer_days(now);
            let of = |name: &str| days.iter().find(|(n, _)| n.ends_with(name)).and_then(|(_, d)| *d).unwrap();
            let saturday = of("weekend");
            assert!((1..=7).contains(&saturday) && (weekday + saturday) % 7 == 6, "weekday {weekday}: {saturday}");
            let monday = of("Next week");
            assert!((1..=7).contains(&monday) && (weekday + monday) % 7 == 1, "weekday {weekday}: {monday}");
            assert_eq!(days.iter().any(|(n, _)| *n == "Next weekend"), weekday == 6 || weekday == 0);
        }
    }

    use super::Choice;
    use crate::state::{ProjectState, State};
    use crate::workspace::Workspace;
    use gpui_kit::{AppContext as _, Bounds, Point, TestAppContext, WindowBounds, WindowOptions, px, size};
    use std::{os::unix::fs::PermissionsExt as _, path::Path, time::Duration};

    /// A task whose agent (a stand-in for Claude Code that keeps a
    /// transcript where Claude Code would) left a commit on no remote and a
    /// new file: an archive that cannot be written leaves it as it was;
    /// then it is archived (worktree and local branch gone, the file
    /// there), and restored from origin with the commit and the file back,
    /// the transcript under the new worktree's folder, the agent resuming
    /// that same session and told what happened first.
    ///
    /// `cargo test -- --ignored an_archived_task` after changing archive or restore.
    #[gpui_kit::test]
    #[ignore]
    fn an_archived_task_comes_back_with_its_conversation(cx: &mut TestAppContext) {
        use crate::agent::HookEvent;
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        cx.executor().allow_parking();
        let dir = std::env::temp_dir().join(format!("ideae-archived-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["bin", "out", "claude"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let sh = |at: &Path, script: &str| {
            assert!(std::process::Command::new("sh").arg("-ec").arg(script).current_dir(at).status().unwrap().success(), "{script}");
        };
        sh(&dir, "git init -q --bare -b main remote.git; git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t
            echo one > f.txt; git add f.txt; git commit -qm base; git remote add origin ../remote.git; git push -q origin main; git fetch -q origin");
        let repo = dir.join("repo").canonicalize().unwrap();
        // Claude Code's stand-in: how it was started, a transcript under
        // its session's id where Claude Code keeps it, then what it is told.
        let claude = dir.join("bin/claude");
        std::fs::write(
            &claude,
            r#"#!/bin/sh
name=$(basename "$PWD")
echo "$@" >> "$OUT/$name.args"
id=""; prev=""
for a in "$@"; do case "$prev" in --session-id|--resume) id="$a";; esac; prev="$a"; done
d="$CLAUDE_CONFIG_DIR/projects/$(printf %s "$PWD" | sed 's/[^A-Za-z0-9]/-/g')"
mkdir -p "$d"
[ -f "$d/$id.jsonl" ] || printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Made the form."}]}}\n' > "$d/$id.jsonl"
exec cat > "$OUT/$name.told"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();

        // SAFETY: set before anything of the app reads them.
        unsafe {
            std::env::set_var("IDEAE_STATE", dir.join("state.json"));
            std::env::set_var("IDEAE_NO_DAEMON", "1");
        }
        let saved = State { projects: vec![ProjectState { path: repo.clone(), ..Default::default() }], ..Default::default() };
        std::fs::write(dir.join("state.json"), serde_json::to_string(&saved).unwrap()).unwrap();
        let mut settings = crate::state::Settings { notifications: false, pr_status: false, fetch_minutes: 0, ..Default::default() };
        let env = [("CLAUDE_CONFIG_DIR", dir.join("claude")), ("OUT", dir.join("out"))].map(|(k, v)| (k.to_string(), v.display().to_string()));
        settings.agents = vec![crate::agents::Profile { command: claude.display().to_string(), env: env.into(), ..crate::agents::Profile::claude_code() }];
        crate::state::set_settings(settings);
        cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_cn::init(cx);
            crate::theme::init(cx);
        });
        let (handle, workspace) = cx.update(|cx| {
            let bounds = Bounds { origin: Point::default(), size: size(px(1300.), px(800.)) };
            let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(Vec::new(), None, None, window, cx))).expect("a test window")
        });
        let step = |cx: &mut TestAppContext, act: &mut dyn FnMut(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>)| {
            cx.update_window(handle.into(), |_, window, cx| workspace.update(cx, |w, cx| act(w, window, cx))).unwrap();
            cx.run_until_parked();
        };
        let wait = |cx: &mut TestAppContext, what: &str, done: &mut dyn FnMut(&mut TestAppContext) -> bool| {
            for _ in 0..1500 {
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
                cx.executor().advance_clock(Duration::from_millis(10));
            }
            panic!("waited in vain for {what}");
        };
        let out = |path: &Path, what: &str| std::fs::read_to_string(dir.join("out").join(format!("{}.{what}", path.file_name().unwrap().to_string_lossy()))).unwrap_or_default();
        let task = |cx: &mut TestAppContext| workspace.update(cx, |w, _| w.projects[0].tasks.iter().find(|t| !t.main).map(|t| (t.path.clone(), t.branch.clone(), t.busy.is_some(), t.view.is_some())));

        step(cx, &mut |w, window, cx| w.create_task(0, "Fix login".into(), "Make the form.".into(), vec![String::new()], vec!["claude".into()], window, cx));
        let mut made = None;
        wait(cx, "the task", &mut |cx| {
            made = task(cx).filter(|t| !t.2 && t.3);
            made.is_some()
        });
        let (path, branch, ..) = made.unwrap();
        wait(cx, "its agent", &mut |_| !out(&path, "args").is_empty());
        let id = workspace.update(cx, |w, _| w.projects[0].tasks.iter().find(|t| !t.main).unwrap().sessions[0].id.clone().unwrap());
        sh(&path, "echo two >> f.txt; git commit -qam 'not pushed'; echo kept > new.txt");

        // Nowhere to write it: nothing goes.
        std::fs::write(dir.join("a-file"), "").unwrap();
        step(cx, &mut |w, window, cx| {
            w.archive_dir = dir.join("a-file/archive");
            w.archive_task(&path, window, cx);
        });
        wait(cx, "the archive to fail", &mut |cx| task(cx).is_some_and(|t| !t.2));
        assert!(path.join("new.txt").is_file() && crate::git::branch_exists(&repo, &branch), "the worktree and its branch untouched");

        let archives = dir.join("archive");
        step(cx, &mut |w, window, cx| {
            w.archive_dir = archives.clone();
            w.archive_task(&path, window, cx);
        });
        wait(cx, "the task to be archived", &mut |cx| task(cx).is_none());
        assert!(!path.exists() && !crate::git::branch_exists(&repo, &branch), "its worktree and local branch gone");
        let listed = crate::archive::list(&archives);
        assert_eq!(listed.len(), 1);
        let meta = listed[0].meta.clone();
        assert_eq!((meta.title.as_str(), meta.prompt.as_str(), meta.said.as_deref()), ("Fix login", "Make the form.", Some("Made the form.")));
        assert_eq!((meta.unpushed.len(), meta.uncommitted.clone()), (1, vec!["new.txt".to_string()]));

        let item = listed[0].clone();
        step(cx, &mut |w, window, cx| w.restore_archived(item.clone(), Choice { pr: Some(None), on_branch: false, commits: true, changes: true, summary: false, branch: String::new() }, false, window, cx));
        let mut back = None;
        wait(cx, "the restored task", &mut |cx| {
            back = task(cx).filter(|t| !t.2 && t.3);
            back.is_some()
        });
        let (again, ..) = back.unwrap();
        assert_ne!(again, path, "a new worktree");
        // Asked again: the task that is here, not a second one writing the same archive.
        let mut asked = true;
        step(cx, &mut |w, window, cx| {
            w.restore_dialog(item.clone(), window, cx);
            asked = gpui_kit::component::WindowExt::has_active_dialog(window, cx);
        });
        assert!(!asked, "no Restore dialog for an archive whose task is here");
        let subject = std::process::Command::new("git").args(["log", "-1", "--format=%s"]).current_dir(&again).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&subject.stdout).trim(), "not pushed");
        assert_eq!(std::fs::read_to_string(again.join("new.txt")).unwrap(), "kept\n");
        let transcript = dir.join("claude/projects").join(crate::agents::claude::project_dir(&again)).join(format!("{id}.jsonl"));
        assert!(std::fs::read_to_string(&transcript).unwrap().contains("Made the form."), "the conversation under the new folder, by its id");
        wait(cx, "the agent to start", &mut |_| !out(&again, "args").is_empty());
        assert!(out(&again, "args").contains(&format!("--resume {id}")), "{}", out(&again, "args"));

        // It is up: the note goes in first.
        let up = HookEvent { task: again.clone(), event: "SessionStart".into(), message: None, kind: None, file: None, said: None, replayed: None, session: Some(id.clone()), guessed: false, worktree: None };
        step(cx, &mut |w, window, cx| w.on_hook(up.clone(), window, cx));
        wait(cx, "the note to reach the agent", &mut |_| out(&again, "told").contains("comes in my next message"));
        let told = out(&again, "told");
        assert!(told.contains("archived on") && told.contains("no pull request") && told.contains("not pushed") && told.contains("applied again"), "{told}");

        // Deferred with its day come: it goes as Archive does and stays from the clean-up.
        let since = crate::state::now();
        step(cx, &mut |w, window, cx| w.archive_now(&again, Some(crate::archive::Deferred { since, until: Some(since) }), window, cx));
        wait(cx, "the task to be deferred", &mut |cx| task(cx).is_none());
        let listed = crate::archive::list(&archives);
        assert_eq!(listed.len(), 1, "in the place of the archive it came from");
        assert!(listed[0].deferred.is_some_and(|d| d.due(since)));
        assert_eq!(crate::archive::clean_up(&archives, 1, since + 86_400), 0, "a deferred task is not cleaned up");
        // Its day: back, paused, nothing started till it is opened.
        step(cx, &mut |w, window, cx| w.wake_deferred(window, cx));
        let mut woke = None;
        wait(cx, "the deferred task to come back", &mut |cx| {
            woke = task(cx).filter(|t| !t.2);
            woke.is_some()
        });
        let (woke, ..) = woke.unwrap();
        assert!(task(cx).is_some_and(|t| !t.3), "paused: no agent, no terminals");
        assert!(workspace.update(cx, |w, _| w.projects[0].tasks.iter().any(|t| t.woke.is_some())));
        assert!(crate::archive::list(&archives)[0].deferred.is_none(), "it does not come back twice");
        step(cx, &mut |w, window, cx| w.wake_deferred(window, cx));
        assert_eq!(workspace.update(cx, |w, _| w.projects[0].tasks.iter().filter(|t| !t.main).count()), 1);
        assert_eq!(std::fs::read_to_string(woke.join("new.txt")).unwrap(), "kept\n");
        // Opened: its agent resumes the conversation and is told what happened.
        step(cx, &mut |w, window, cx| w.switch(&woke, window, cx));
        wait(cx, "the agent to start", &mut |_| !out(&woke, "args").is_empty());
        assert!(out(&woke, "args").contains(&format!("--resume {id}")), "{}", out(&woke, "args"));
        let up = HookEvent { task: woke.clone(), ..up.clone() };
        step(cx, &mut |w, window, cx| w.on_hook(up.clone(), window, cx));
        wait(cx, "the note to reach the agent", &mut |_| out(&woke, "told").contains("comes in my next message"));
        assert!(workspace.update(cx, |w, _| w.projects[0].tasks.iter().all(|t| t.woke.is_none())), "told once");

        step(cx, &mut |w, _, cx| {
            for view in w.views() {
                view.update(cx, |t, cx| t.shut_down_at_quit(cx));
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
