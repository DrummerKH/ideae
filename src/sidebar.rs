//! The left sidebar: a search, the tasks that need you, then each project
//! with its tasks. Rows drag to reorder; right-click for the rest.

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::Input,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
    h_flex, v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use crate::{agent::AgentStatus, task, workspace::Workspace};

#[derive(Clone)]
pub enum DraggedRow {
    Project { ix: usize, name: SharedString },
    Task { project: usize, path: PathBuf, name: SharedString },
}

impl Render for DraggedRow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = match self {
            DraggedRow::Project { name, .. } | DraggedRow::Task { name, .. } => name.clone(),
        };
        div()
            .px_3()
            .py_1()
            .text_sm()
            .rounded_md()
            .bg(cx.theme().secondary)
            .border_1()
            .border_color(cx.theme().primary)
            .child(name)
    }
}

fn section_header(label: &'static str, cx: &App) -> Div {
    h_flex()
        .px_3()
        .pt_3()
        .pb_1()
        .justify_between()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

/// "now", "5m", "3h", "2d", "2w", "4mo".
fn ago(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    let d = crate::state::now().saturating_sub(secs);
    match d {
        0..60 => "now".into(),
        60..3600 => format!("{}m", d / 60),
        3600..86400 => format!("{}h", d / 3600),
        86400..1_209_600 => format!("{}d", d / 86400),
        1_209_600..5_184_000 => format!("{}w", d / 604_800),
        _ => format!("{}mo", d / 2_592_000),
    }
}

/// Claude puts a spinner or mark before its session title ("✳ Fix login").
fn clean_title(title: &str) -> String {
    title.trim_start_matches(|c: char| !c.is_alphanumeric()).trim().to_string()
}

impl Workspace {
    pub fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let mut list = v_flex().id("sidebar-list").flex_1().min_h_0().overflow_y_scroll().pb_2();

        if query.is_empty() {
            let active = self.active_rows(cx);
            if !active.is_empty() {
                let collapsed = self.active_collapsed;
                list = list.child(
                    section_header(if collapsed { "▸ NEEDS YOU" } else { "▾ NEEDS YOU" }, cx)
                        .id("active-header")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.active_collapsed = !this.active_collapsed;
                            cx.notify();
                        })),
                );
                if !collapsed {
                    for (pi, ti) in active {
                        list = list.child(self.render_task(pi, ti, true, cx));
                    }
                }
            }
        }

        if self.projects.is_empty() {
            list = list.child(
                div().px_3().py_3().child(
                    Button::new("add-first-project")
                        .small()
                        .outline()
                        .label("Add a project…")
                        .on_click(cx.listener(|this, _, window, cx| this.add_project_dialog(window, cx))),
                ),
            );
        }
        let mut shown = 0;
        for pi in 0..self.projects.len() {
            let p = &self.projects[pi];
            let project_hit = query.is_empty() || p.name.to_lowercase().contains(&query);
            let tasks: Vec<usize> = (0..p.tasks.len())
                .filter(|&ti| project_hit || self.task_matches(pi, ti, &query, cx))
                .collect();
            if !query.is_empty() && !project_hit && tasks.is_empty() {
                continue;
            }
            list = list.child(self.render_project(pi, shown == 0, cx));
            shown += 1;
            // A search shows its hits even in folded projects.
            if !p.collapsed || !query.is_empty() {
                for ti in tasks {
                    list = list.child(self.render_task(pi, ti, false, cx));
                }
            }
        }
        // Dropping below the last project moves it to the end.
        list = list.child(
            div()
                .id("sidebar-end")
                .flex_1()
                .min_h(px(24.))
                .drag_over::<DraggedRow>(|s, row, _, cx| match row {
                    DraggedRow::Project { .. } => s.border_t_2().border_color(cx.theme().primary),
                    _ => s,
                })
                .on_drop(cx.listener(|this, row: &DraggedRow, _, cx| {
                    if let DraggedRow::Project { ix, .. } = row {
                        let end = this.projects.len();
                        this.move_project(*ix, end, cx);
                    }
                })),
        );

        v_flex()
            .size_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1p5()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().child(Input::new(&self.search).xsmall().appearance(false).cleanable(true)))
                    .child(
                        Button::new("add-project")
                            .xsmall()
                            .ghost()
                            .label("+")
                            .tooltip("Add project")
                            .on_click(cx.listener(|this, _, window, cx| this.add_project_dialog(window, cx))),
                    ),
            )
            .child(list)
    }

    fn task_matches(&self, pi: usize, ti: usize, query: &str, cx: &App) -> bool {
        let t = &self.projects[pi].tasks[ti];
        let title = self.task_title(pi, ti, cx).to_lowercase();
        title.contains(query) || t.branch.to_lowercase().contains(query)
    }

    /// What a task is called in the list: its own title, or for the main
    /// thread (and untitled worktrees) the agent's session title.
    fn task_title(&self, pi: usize, ti: usize, cx: &App) -> String {
        let t = &self.projects[pi].tasks[ti];
        if !t.main && !t.title.is_empty() {
            return t.title.clone();
        }
        let agent = t
            .view
            .as_ref()
            .and_then(|v| v.read(cx).agent_title(cx))
            .map(|s| clean_title(&s))
            // Until the agent names itself the terminal shows the command line.
            .filter(|s| !s.is_empty() && !s.contains(" --settings ") && s != "Claude Code");
        agent.unwrap_or_else(|| if t.main { "main".into() } else { t.branch.clone() })
    }

    /// Tasks whose agent waits for an answer or finished while out of view:
    /// an inbox, empty most of the time, most recent first.
    fn active_rows(&self, cx: &App) -> Vec<(usize, usize)> {
        let mut rows: Vec<(usize, usize)> = Vec::new();
        for (pi, p) in self.projects.iter().enumerate() {
            for (ti, t) in p.tasks.iter().enumerate() {
                let waiting = t.view.as_ref().is_some_and(|v| {
                    let v = v.read(cx);
                    v.unseen || matches!(v.agent_status, AgentStatus::NeedsInput(_))
                });
                if waiting && self.active.as_deref() != Some(t.path.as_path()) {
                    rows.push((pi, ti));
                }
            }
        }
        rows.sort_by_key(|&(pi, ti)| std::cmp::Reverse(self.projects[pi].tasks[ti].last_active));
        rows
    }

    fn render_project(&self, pi: usize, first: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = &self.projects[pi];
        let name: SharedString = p.name.clone().into();
        let running = p.tasks.iter().filter(|t| t.view.is_some()).count();
        let group = SharedString::from(format!("project-group-{pi}"));
        let muted = cx.theme().muted_foreground;
        let this = cx.weak_entity();
        let menu = move |menu: PopupMenu, this: WeakEntity<Workspace>| {
            let (a, b, c, d) = (this.clone(), this.clone(), this.clone(), this);
            menu.item(PopupMenuItem::new("Open main thread").on_click(move |_, window, cx| {
                let _ = a.update(cx, |w, cx| w.open_main(pi, window, cx));
            }))
            .item(PopupMenuItem::new("New task…").on_click(move |_, window, cx| {
                let _ = b.update(cx, |w, cx| w.new_task_dialog(pi, window, cx));
            }))
            .item(PopupMenuItem::new("Clean up merged tasks…").on_click(move |_, window, cx| {
                let _ = d.update(cx, |w, cx| w.clean_up_merged(pi, window, cx));
            }))
            .separator()
            .item(PopupMenuItem::new("Remove from list…").on_click(move |_, window, cx| {
                let _ = c.update(cx, |w, cx| w.remove_project(pi, window, cx));
            }))
        };
        let (menu_a, this_a) = (menu.clone(), this.clone());
        h_flex()
            .id(SharedString::from(format!("project-{}", p.path.display())))
            .group(group.clone())
            .pl_3()
            .pr_1()
            .pt_2()
            .pb_1()
            .gap_1()
            .when(!first, |s| s.mt_1().border_t_1().border_color(cx.theme().border))
            .cursor_pointer()
            .text_sm()
            .text_color(muted)
            .child(div().flex_1().min_w_0().truncate().child(name.clone()))
            .when(p.collapsed, |s| {
                s.child(div().text_xs().child(if running > 0 { format!("{running} running ▸") } else { "▸".into() }))
            })
            .child(
                div()
                    .opacity(0.)
                    .group_hover(group, |s| s.opacity(1.))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new(SharedString::from(format!("project-add-{pi}")))
                            .xsmall()
                            .ghost()
                            .label("+")
                            .dropdown_menu(move |m, _, _| menu_a(m, this_a.clone())),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(p) = this.projects.get_mut(pi) {
                    p.collapsed = !p.collapsed;
                }
                cx.notify();
            }))
            .on_drag(DraggedRow::Project { ix: pi, name: name.clone() }, |row, _, _, cx| cx.new(|_| row.clone()))
            .drag_over::<DraggedRow>(move |s, row, _, cx| match row {
                DraggedRow::Project { .. } => s.border_t_2().border_color(cx.theme().primary),
                DraggedRow::Task { project, .. } if *project == pi => s.border_b_2().border_color(cx.theme().primary),
                _ => s,
            })
            .on_drop(cx.listener(move |this, row: &DraggedRow, _, cx| match row {
                DraggedRow::Project { ix, .. } => this.move_project(*ix, pi, cx),
                DraggedRow::Task { project, path, .. } => this.move_task(path, *project, Some(0), cx),
            }))
            .context_menu(move |m, _, _| menu(m, this.clone()))
            .into_any_element()
    }

    fn render_task(&self, pi: usize, ti: usize, in_active: bool, cx: &mut Context<Self>) -> AnyElement {
        let project = &self.projects[pi];
        let t = &project.tasks[ti];
        let path = t.path.clone();
        let label: SharedString = self.task_title(pi, ti, cx).into();
        let is_active = self.active.as_deref() == Some(path.as_path());
        let running = t.view.is_some();

        let (glyph, changes, unseen, needs) = match &t.view {
            Some(view) => {
                let v = view.read(cx);
                let needs = match &v.agent_status {
                    AgentStatus::NeedsInput(message) => Some(message.clone()),
                    _ => None,
                };
                (task::status_glyph(&v.agent_status), v.change_count(), v.unseen, needs)
            }
            None => (None, 0, false, None),
        };
        let muted = cx.theme().muted_foreground;
        // Second line: when it was last busy, and where (the project in the
        // inbox, the branch when the title is not it).
        let mut details: Vec<String> = Vec::new();
        let when = ago(t.last_active);
        if !when.is_empty() {
            details.push(when);
        }
        if in_active {
            details.push(project.name.clone());
        } else if t.main {
            details.push(format!("⌂ {}", t.branch));
        } else if label.as_ref() != t.branch {
            details.push(t.branch.clone());
        }
        if t.starred {
            details.push("★".into());
        }
        if t.locked.is_some() {
            details.push("locked".into());
        }
        if let Some(busy) = &t.busy {
            details.push(busy.to_string());
        }
        let needs = if in_active { None } else { needs };
        let pr = t.pr.clone();

        let this = cx.weak_entity();
        let (starred, main) = (t.starred, t.main);
        let menu_path = path.clone();
        let branch = t.branch.clone();
        let menu = move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
            let (a, b, c) = (this.clone(), this.clone(), this.clone());
            let (pa, pb, pc) = (menu_path.clone(), menu_path.clone(), menu_path.clone());
            let branch = branch.clone();
            menu.item(PopupMenuItem::new(if starred { "Unstar" } else { "Star" }).on_click(move |_, _, cx| {
                let _ = a.update(cx, |w, cx| w.toggle_star(&pa, cx));
            }))
            .when(running, |m| {
                m.item(PopupMenuItem::new(if main { "Close main thread" } else { "Close" }).on_click(
                    move |_, window, cx| {
                        let _ = b.update(cx, |w, cx| w.close_task(&pb, window, cx));
                    },
                ))
            })
            .item(PopupMenuItem::new("Copy branch name").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(branch.clone()));
            }))
            .when(!main, |m| {
                m.separator().item(PopupMenuItem::new("Delete…").on_click(move |_, window, cx| {
                    let _ = c.update(cx, |w, cx| w.delete_task(&pc, window, cx));
                }))
            })
        };

        let click_path = path.clone();
        let drop_path = path.clone();
        let id = SharedString::from(format!("{}task-{}", if in_active { "active-" } else { "" }, path.display()));
        let mut row = v_flex()
            .id(id)
            .pl_3()
            .pr_3()
            .py_1()
            .cursor_pointer()
            .border_l_2()
            .border_color(transparent_black())
            .when(is_active, |s| s.bg(cx.theme().accent).border_color(cx.theme().primary))
            .when(unseen && !is_active, |s| s.border_color(glyph.map(|g| g.1).unwrap_or(cx.theme().border)))
            .hover(|s| s.bg(cx.theme().accent.opacity(0.6)))
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .flex_none()
                            .w(px(10.))
                            .text_xs()
                            .when_some(glyph, |s, (g, color)| s.text_color(color).child(g)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .when(!running, |s| s.text_color(muted))
                            .child(label.clone()),
                    )
                    .when(changes > 0, |s| s.child(div().flex_none().text_xs().text_color(muted).child(changes.to_string()))),
            )
            .when(!details.is_empty() || pr.is_some(), |s| {
                s.child(
                    h_flex()
                        .pl(px(18.))
                        .gap_1p5()
                        .min_w_0()
                        .text_xs()
                        .when_some(pr, |row, pr| {
                            use crate::pr::{Checks, PrState};
                            let color = match (pr.state, pr.checks) {
                                (PrState::Merged, _) => hsla(0.75, 0.5, 0.62, 1.),
                                (PrState::Open, Checks::Passed) => hsla(0.36, 0.6, 0.45, 1.),
                                (PrState::Open, Checks::Failed) => hsla(0.0, 0.7, 0.55, 1.),
                                (PrState::Open, _) => hsla(0.11, 0.8, 0.5, 1.),
                                _ => muted,
                            };
                            let url = pr.url.clone();
                            row.child(
                                div()
                                    .id(SharedString::from(format!("pr-{}", pr.url)))
                                    .flex_none()
                                    .text_color(color)
                                    .hover(|s| s.underline())
                                    .child(pr.label())
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        cx.open_url(&url);
                                    }),
                            )
                        })
                        .when(!details.is_empty(), |row| {
                            row.child(div().min_w_0().truncate().text_color(muted).child(details.join(" · ")))
                        }),
                )
            })
            .when_some(needs, |s, message| {
                s.child(div().pl(px(18.)).text_xs().truncate().text_color(hsla(0.08, 0.9, 0.55, 1.)).child(message))
            })
            // A click on a task, current or not, lands in its agent.
            .on_click(cx.listener(move |this, _, window, cx| {
                this.switch(&click_path, window, cx);
                if let Some(view) = this.active_view() {
                    view.update(cx, |t, cx| t.show_view(0, window, cx));
                }
            }));
        if !in_active {
            row = row
                .on_drag(DraggedRow::Task { project: pi, path: path.clone(), name: label }, |row, _, _, cx| {
                    cx.new(|_| row.clone())
                })
                .drag_over::<DraggedRow>(move |s, row, _, cx| match row {
                    DraggedRow::Task { project, .. } if *project == pi => s.border_t_2().border_color(cx.theme().primary),
                    _ => s,
                })
                .on_drop(cx.listener(move |this, row: &DraggedRow, _, cx| {
                    if let DraggedRow::Task { project, path, .. } = row {
                        if let Some((_, to)) = this.find(&drop_path) {
                            this.move_task(path, *project, Some(to), cx);
                        }
                    }
                }));
        }
        row.context_menu(menu).into_any_element()
    }
}
