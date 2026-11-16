//! Presenting: the window shown to others has only the tasks chosen for
//! it. The rest go on as they were, out of the list, the palette and the
//! notifications, until the presentation ends.

use std::{
    cell::RefCell,
    collections::HashSet,
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    notification::Notification,
    v_flex,
};
use gpui_kit::*;

use super::Workspace;
use crate::state;

/// Whether a presentation is on, for the views that hide what is theirs.
static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn on() -> bool {
    ON.load(std::sync::atomic::Ordering::Relaxed)
}

/// What is shown while presenting.
pub struct Presenting {
    /// The tasks chosen, by worktree.
    pub kept: HashSet<PathBuf>,
    /// When it began: a task made after that in a project that is shown
    /// is shown too.
    pub since: u64,
    /// Tasks out of sight that asked for you meanwhile.
    held: HashSet<PathBuf>,
}

impl Presenting {
    pub fn new(kept: HashSet<PathBuf>, since: u64) -> Self {
        Self { kept, since, held: HashSet::new() }
    }
}

impl Workspace {
    pub fn is_presenting(&self) -> bool {
        self.presenting.is_some()
    }

    /// Whether a task is to be seen: always, unless a presentation is on
    /// and the task was not chosen for it.
    pub fn shows(&self, pi: usize, ti: usize) -> bool {
        // First of all, of the space in the window.
        if !self.in_space(pi) {
            return false;
        }
        let Some(presenting) = &self.presenting else { return true };
        let (project, task) = (&self.projects[pi], &self.projects[pi].tasks[ti]);
        presenting.kept.contains(&task.path) || (task.created >= presenting.since && project.tasks.iter().any(|t| presenting.kept.contains(&t.path)))
    }

    pub fn shows_path(&self, path: &Path) -> bool {
        self.find(path).is_none_or(|(pi, ti)| self.shows(pi, ti))
    }

    /// A task out of sight wanted you: told of when the presentation ends.
    pub fn hold_notice(&mut self, path: &Path) {
        if let Some(presenting) = &mut self.presenting {
            presenting.held.insert(path.to_path_buf());
        }
    }

    /// Asks which tasks to leave in the window, the ones of the project in
    /// hand ticked to start with.
    pub fn presentation_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let here = self.active_project();
        let rows: Vec<(String, Vec<(PathBuf, String)>)> = self
            .projects
            .iter()
            .enumerate()
            // Of the space in the window: the others are not there to be shown.
            .filter(|(pi, _)| self.in_space(*pi))
            .map(|(pi, p)| (p.name.clone(), (0..p.tasks.len()).map(|ti| (p.tasks[ti].path.clone(), self.task_title(pi, ti, cx))).collect()))
            .collect();
        let chosen: HashSet<PathBuf> = here.map(|pi| self.projects[pi].tasks.iter().map(|t| t.path.clone()).collect()).unwrap_or_default();
        let chosen = Rc::new(RefCell::new(chosen));
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let (rows, chosen, this) = (rows.clone(), chosen.clone(), this.clone());
            let muted = cx.theme().muted_foreground;
            dialog.title("Presentation mode").w(px(520.)).content(move |content, _, _| {
                let mut list = v_flex().id("present-tasks").gap_1().max_h(px(420.)).overflow_y_scroll();
                for (project, tasks) in &rows {
                    list = list.child(div().pt_2().text_xs().text_color(muted).child(project.to_uppercase()));
                    for (path, title) in tasks {
                        let (toggle, path) = (chosen.clone(), path.clone());
                        let on = chosen.borrow().contains(&path);
                        list = list.child(
                            Checkbox::new(SharedString::from(format!("present-{}", path.display()))).label(title.clone()).checked(on).on_click(move |checked, window, _| {
                                match *checked {
                                    true => toggle.borrow_mut().insert(path.clone()),
                                    false => toggle.borrow_mut().remove(&path),
                                };
                                window.refresh();
                            }),
                        );
                    }
                }
                let (go, this) = (chosen.clone(), this.clone());
                let none = chosen.borrow().is_empty();
                content
                    .child(div().text_sm().text_color(muted).child(
                        "Only the tasks ticked stay in the window: the others leave the list, the search and the notifications, and go on working. The load and token readouts are hidden too.",
                    ))
                    .child(list)
                    .child(h_flex().pt_3().justify_end().child(Button::new("present-go").small().primary().label("Present").disabled(none).on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let kept = go.borrow().clone();
                        let _ = this.update(cx, |w, cx| w.present(kept, window, cx));
                    })))
            })
        });
    }

    pub fn present(&mut self, kept: HashSet<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if kept.is_empty() {
            return;
        }
        // The task in hand is not one to show: the first that is takes its place.
        let in_hand = self.active.clone().is_some_and(|path| kept.contains(&path));
        let first = self.projects.iter().flat_map(|p| &p.tasks).map(|t| t.path.clone()).find(|path| kept.contains(path));
        self.presenting = Some(Presenting::new(kept, state::now()));
        self.apply_presenting(cx);
        if !in_hand {
            if let Some(path) = first {
                self.switch(&path, window, cx);
            }
        }
        self.save_later(cx);
        cx.notify();
    }

    /// Everything is back in the window, with what was missed meanwhile.
    pub fn end_presentation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(presenting) = self.presenting.take() else { return };
        self.apply_presenting(cx);
        let waited: Vec<String> = presenting
            .held
            .iter()
            .filter_map(|path| self.find(path))
            .map(|(pi, ti)| format!("{} · {}", self.projects[pi].name, self.projects[pi].tasks[ti].label()))
            .collect();
        if !waited.is_empty() {
            let told = format!("While you presented, {} asked for you or finished:\n{}", if waited.len() == 1 { "a task".into() } else { format!("{} tasks", waited.len()) }, waited.join("\n"));
            window.push_notification(Notification::info(told).title("Presentation ended"), cx);
        }
        self.save_later(cx);
        cx.notify();
    }

    /// What follows from a presentation being on or off: text of the size
    /// for it, and the files git ignores (keys, local settings) not shown
    /// until asked for.
    pub fn apply_presenting(&mut self, cx: &mut Context<Self>) {
        let on = self.presenting.is_some();
        ON.store(on, std::sync::atomic::Ordering::Relaxed);
        let factor = if on { state::with_settings(|s| s.present_zoom.clamp(50, 300)) as f32 / 100. } else { 1. };
        crate::zoom::set_scale(factor, cx);
        self.tell_what_is_hidden();
        for view in self.views() {
            view.update(cx, |task, cx| task.veil_ignored(cx));
        }
        cx.refresh_windows();
    }

    /// In the title bar while presenting: that it is on, and the way out.
    pub fn render_presenting(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let presenting = self.presenting.as_ref()?;
        let held = presenting.held.len();
        Some(
            Button::new("presenting")
                .xsmall()
                .outline()
                .label(if held == 0 { "Presenting ×".to_string() } else { format!("Presenting · {held} wait ×") })
                .tooltip("Only the tasks chosen are in the window. A click ends the presentation and brings the rest back.")
                .on_click(cx.listener(|this, _, window, cx| this.end_presentation(window, cx)))
                .into_any_element(),
        )
    }
}
