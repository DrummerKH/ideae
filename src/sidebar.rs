//! The left sidebar: recently active tasks on top, then each project with its
//! tasks. Rows drag to reorder; right-click for the rest.

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
    h_flex, v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use crate::{agent::AgentStatus, state, task, workspace::Workspace};

/// How long a task stays under Active after it was last in view.
const ACTIVE_WINDOW: u64 = 24 * 3600;

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

impl Workspace {
    pub fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex()
            .id("sidebar")
            .size_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(cx.theme().border);

        let active = self.active_rows();
        if !active.is_empty() {
            let collapsed = self.active_collapsed;
            list = list.child(
                section_header(if collapsed { "▸ ACTIVE" } else { "▾ ACTIVE" }, cx)
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

        list = list.child(
            section_header("PROJECTS", cx).child(
                Button::new("add-project")
                    .xsmall()
                    .ghost()
                    .label("+")
                    .tooltip("Add project")
                    .on_click(cx.listener(|this, _, window, cx| this.add_project_dialog(window, cx))),
            ),
        );
        if self.projects.is_empty() {
            list = list.child(
                div().px_3().py_2().child(
                    Button::new("add-first-project")
                        .small()
                        .outline()
                        .label("Add a project…")
                        .on_click(cx.listener(|this, _, window, cx| this.add_project_dialog(window, cx))),
                ),
            );
        }
        for pi in 0..self.projects.len() {
            list = list.child(self.render_project(pi, cx));
            if !self.projects[pi].collapsed {
                for ti in 0..self.projects[pi].tasks.len() {
                    list = list.child(self.render_task(pi, ti, false, cx));
                }
            }
        }
        // Dropping below the last project moves it to the end.
        list.child(
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
        )
    }

    /// Starred tasks, running ones and those in view within the last day;
    /// starred first, then the most recent.
    fn active_rows(&self) -> Vec<(usize, usize)> {
        let since = state::now().saturating_sub(ACTIVE_WINDOW);
        let mut rows: Vec<(usize, usize)> = Vec::new();
        for (pi, p) in self.projects.iter().enumerate() {
            for (ti, t) in p.tasks.iter().enumerate() {
                if t.starred || t.view.is_some() || t.last_active >= since {
                    rows.push((pi, ti));
                }
            }
        }
        rows.sort_by_key(|&(pi, ti)| {
            let t = &self.projects[pi].tasks[ti];
            (!t.starred, std::cmp::Reverse(t.last_active))
        });
        rows
    }

    fn render_project(&self, pi: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = &self.projects[pi];
        let name: SharedString = p.name.clone().into();
        let running = p.tasks.iter().filter(|t| t.view.is_some()).count();
        let this = cx.weak_entity();
        let menu = move |menu: PopupMenu, this: WeakEntity<Workspace>| {
            let (a, b, c) = (this.clone(), this.clone(), this);
            menu.item(PopupMenuItem::new("Open main thread").on_click(move |_, window, cx| {
                let _ = a.update(cx, |w, cx| w.open_main(pi, window, cx));
            }))
            .item(PopupMenuItem::new("New task…").on_click(move |_, window, cx| {
                let _ = b.update(cx, |w, cx| w.new_task_dialog(pi, window, cx));
            }))
            .separator()
            .item(PopupMenuItem::new("Remove from list…").on_click(move |_, window, cx| {
                let _ = c.update(cx, |w, cx| w.remove_project(pi, window, cx));
            }))
        };
        let (menu_a, this_a) = (menu.clone(), this.clone());
        h_flex()
            .id(SharedString::from(format!("project-{}", p.path.display())))
            .px_2()
            .py_1()
            .mt_1()
            .gap_1()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().accent))
            .child(
                div()
                    .w_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if p.collapsed { "▸" } else { "▾" }),
            )
            .child(div().flex_1().min_w_0().truncate().text_sm().font_semibold().child(name.clone()))
            .when(p.collapsed && running > 0, |s| {
                s.child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!("{running} running")))
            })
            .child(
                div()
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
        let label: SharedString = if t.main { format!("⌂ {}", t.branch).into() } else { t.label().into() };
        let is_active = self.active.as_deref() == Some(path.as_path());
        let running = t.view.is_some();

        let (glyph, changes, unseen, needs, agent_title) = match &t.view {
            Some(view) => {
                let v = view.read(cx);
                let needs = match &v.agent_status {
                    AgentStatus::NeedsInput(message) => Some(message.clone()),
                    _ => None,
                };
                (task::status_glyph(&v.agent_status), v.change_count(), v.unseen, needs, v.agent_title(cx))
            }
            None => (None, 0, false, None, None),
        };
        let agent_title = agent_title.filter(|s| !s.is_empty());
        let muted = cx.theme().muted_foreground;
        let subtitle = match (in_active, t.main || t.title.is_empty()) {
            (true, true) => Some(project.name.clone()),
            (true, false) => Some(format!("{} · {}", project.name, t.branch)),
            (false, true) => None,
            (false, false) => Some(t.branch.clone()),
        };

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
            .pl(px(if in_active { 12. } else { 26. }))
            .pr_3()
            .py_1p5()
            .gap_0p5()
            .cursor_pointer()
            .when(is_active, |s| s.bg(cx.theme().accent))
            .when(unseen, |s| s.border_l_2().border_color(glyph.map(|g| g.1).unwrap_or(cx.theme().border)))
            .hover(|s| s.bg(cx.theme().accent))
            .child(
                h_flex()
                    .gap_1p5()
                    .min_w_0()
                    .when_some(glyph, |s, (g, color)| s.child(div().flex_none().text_color(color).child(g)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .when(running, |s| s.font_semibold())
                            .when(!running, |s| s.text_color(muted))
                            .child(label.clone()),
                    )
                    .when(t.starred, |s| s.child(div().flex_none().text_xs().text_color(muted).child("★")))
                    .when(changes > 0, |s| s.child(div().flex_none().text_xs().text_color(muted).child(changes.to_string())))
                    .when_some(t.busy.clone(), |s, busy| s.child(div().flex_none().text_xs().text_color(muted).child(busy))),
            )
            .when_some(subtitle, |s, sub| s.child(div().text_xs().truncate().text_color(muted).child(sub)))
            .when_some(needs, |s, message| {
                s.child(div().text_xs().truncate().text_color(hsla(0.08, 0.9, 0.55, 1.)).child(message))
            })
            .when_some(agent_title, |s, title| s.child(div().text_xs().truncate().child(title)))
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
