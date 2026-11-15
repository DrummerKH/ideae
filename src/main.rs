mod agent;
mod browser;
mod desktop;
mod finder;
mod git;
mod grammars;
mod history;
mod ide;
mod lsp;
mod naming;
mod search;
mod sidebar;
mod state;
mod task;
mod theme;
mod workspace;

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    resizable::{h_resizable, resizable_panel},
    h_flex, v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use theme::Appearance;
use workspace::Workspace;

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Focus mode: the task's zoomed pane floats over the whole window, a
        // few pixels in from its edges so the rest still shows around it.
        let zoomed_pane = self.active_view().and_then(|task| task.update(cx, |t, cx| t.render_zoomed(cx)));
        v_flex()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_4()
                        .justify_between()
                        .child(
                            h_flex()
                                .id("title-menu")
                                .gap_2()
                                .min_w_0()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(|_, _, cx| cx.stop_propagation())
                                .child(self.render_menu(cx)),
                        )
                        .child(self.render_title(cx))
                        .child(
                            // Clicks on these buttons are not the title bar's: a quick
                            // second one would zoom the window as a double click, and a
                            // slight move while pressed would start dragging it.
                            h_flex()
                                .id("title-actions")
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(|_, _, cx| cx.stop_propagation())
                                .gap_3()
                                .child(
                                    Button::new("go-to-file")
                                        .xsmall()
                                        .ghost()
                                        .label("Go to file  ⌃⇧N")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.finder(finder::FinderKind::Files, window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("find-in-files")
                                        .xsmall()
                                        .ghost()
                                        .label("Find  ⌃⇧F")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.finder(finder::FinderKind::Grep, window, cx)
                                        })),
                                )
                                .child(self.render_appearance(cx)),
                        ),
                ),
            )
            .child(
                div().relative().flex_1().min_h_0().child(
                    h_resizable("workspace")
                        .on_resize({
                            let this = cx.weak_entity();
                            move |state, _, cx| {
                                if let Some(width) = state.read(cx).sizes().first().copied() {
                                    let _ = this.update(cx, |w, _| w.sidebar_width = width);
                                }
                            }
                        })
                        .child(
                            resizable_panel()
                                .size(self.sidebar_width)
                                .size_range(px(150.)..px(520.))
                                .child(self.render_sidebar(cx)),
                        )
                        .child(resizable_panel().child(match self.active_view() {
                            Some(view) => div().size_full().child(view).into_any_element(),
                            None => self.render_empty(cx).into_any_element(),
                        })),
                )
            .when_some(zoomed_pane, |body, pane| {
                    body.child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .p(px(10.))
                            .bg(gpui_kit::black().opacity(0.35))
                            .child(
                                div()
                                    .size_full()
                                    .rounded_md()
                                    .overflow_hidden()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .shadow_lg()
                                    .bg(cx.theme().background)
                                    .child(pane),
                            ),
                    )
                }),
            )

            // Files dropped from a file manager anywhere but a terminal.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                if let Some(task) = this.active_view() {
                    task.update(cx, |t, cx| t.open_dropped(paths.paths(), window, cx));
                }
            }))
            // Tab and panel keys, for the task in view.
            .on_action(cx.listener(|this, a: &task::ShowView, window, cx| {
                if let Some(task) = this.active_view() {
                    task.update(cx, |t, cx| t.show_view(a.0, window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &task::NextTab, window, cx| {
                if let Some(task) = this.active_view() {
                    task.update(cx, |t, cx| t.cycle_tab(true, window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &task::PrevTab, window, cx| {
                if let Some(task) = this.active_view() {
                    task.update(cx, |t, cx| t.cycle_tab(false, window, cx));
                }
            }))
            .on_action(cx.listener(|this, a: &task::ShowSidePanel, window, cx| {
                if let Some(task) = this.active_view() {
                    task.update(cx, |t, cx| t.show_side_panel(a.0, window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &GoToFile, window, cx| this.finder(finder::FinderKind::Files, window, cx)))
            // Reached when nothing inside the task holds focus (a placeholder, a list).
            .on_action(cx.listener(|this, _: &task::ToggleZoom, window, cx| {
                if let Some(task) = this.active_view() {
                    task.update(cx, |t, cx| t.toggle_zoom_focused(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &FindInFiles, window, cx| this.finder(finder::FinderKind::Grep, window, cx)))
            .on_action(cx.listener(|this, _: &NewTask, window, cx| {
                if let Some(project) = this.active_project().or((!this.projects.is_empty()).then_some(0)) {
                    this.new_task_dialog(project, window, cx);
                }
            }))
    }
}

actions!(ideae, [GoToFile, FindInFiles, NewTask]);

impl Workspace {
    fn finder(&mut self, kind: finder::FinderKind, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(task) = self.active_view() {
            task.update(cx, |t, cx| t.open_finder(kind, window, cx));
        }
    }

    /// One button: the current mode's icon, a click moves to the next.
    fn render_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.appearance;
        let next = current.next();
        Button::new("appearance")
            .xsmall()
            .ghost()
            .label(current.icon())
            .tooltip(format!("Theme: {} (click: {})", current.label(), next.label()))
            .on_click(cx.listener(move |this, _, window, cx| this.set_appearance(next, window, cx)))
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
        let this = cx.weak_entity();
        let project = self.active_project();
        let pinned = self.pinned;
        Button::new("app-menu").xsmall().ghost().label("☰").dropdown_menu(move |menu, _, _| {
            let (a, b, c, d) = (this.clone(), this.clone(), this.clone(), this.clone());
            let e = this.clone();
            menu.item(PopupMenuItem::new("Add project…").on_click(move |_, window, cx| {
                let _ = a.update(cx, |w, cx| w.add_project_dialog(window, cx));
            }))
            .when_some(project, |menu, project| {
                menu.item(PopupMenuItem::new("New task…  Ctrl+Shift+T").on_click(move |_, window, cx| {
                    let _ = b.update(cx, |w, cx| w.new_task_dialog(project, window, cx));
                }))
            })
            .separator()
            .item(PopupMenuItem::new(if pinned { "Unpin from dash" } else { "Pin to dash" }).on_click(
                move |_, window, cx| {
                    let result = if pinned { Ok(desktop::unpin()) } else { desktop::pin() };
                    match result {
                        Ok(()) => {
                            let _ = e.update(cx, |w, cx| {
                                w.pinned = !pinned;
                                cx.notify();
                            });
                        }
                        Err(err) => window.push_notification(
                            gpui_kit::component::notification::Notification::error(err).title("Pin to dash"),
                            cx,
                        ),
                    }
                },
            ))
            .item(PopupMenuItem::new("Restart").on_click(move |_, window, cx| {
                let _ = c.update(cx, |w, cx| w.save(cx));
                match desktop::spawn_again() {
                    Ok(()) => cx.quit(),
                    Err(e) => window.push_notification(
                        gpui_kit::component::notification::Notification::error(e.to_string()).title("Restart"),
                        cx,
                    ),
                }
            }))
            .item(PopupMenuItem::new("Exit").on_click(move |_, _, cx| {
                let _ = d.update(cx, |w, cx| w.save(cx));
                cx.quit();
            }))
        })
    }

    /// The project and task in view.
    fn render_title(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .active
            .as_ref()
            .and_then(|a| self.find(a))
            .map(|(pi, ti)| {
                let p = &self.projects[pi];
                let t = &p.tasks[ti];
                if t.main { format!("{} · {}", p.name, t.branch) } else { format!("{} · {}", p.name, t.label()) }
            })
            .unwrap_or_else(|| state::APP_ID.into());
        div().flex_1().min_w_0().px_3().truncate().text_sm().text_color(cx.theme().muted_foreground).child(title)
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project = self.active_project().or((!self.projects.is_empty()).then_some(0));
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .text_color(cx.theme().muted_foreground)
            .child(if self.projects.is_empty() { "Add a project to start." } else { "Pick a task on the left, or start one." })
            .child(
                h_flex()
                    .gap_2()
                    .when(self.projects.is_empty(), |s| {
                        s.child(
                            Button::new("empty-add-project")
                                .small()
                                .outline()
                                .label("Add project…")
                                .on_click(cx.listener(|this, _, window, cx| this.add_project_dialog(window, cx))),
                        )
                    })
                    .when_some(project, |s, project| {
                        s.child(
                            Button::new("empty-main")
                                .small()
                                .outline()
                                .label("Open main thread")
                                .on_click(cx.listener(move |this, _, window, cx| this.open_main(project, window, cx))),
                        )
                        .child(
                            Button::new("empty-new-task")
                                .small()
                                .outline()
                                .label("New task…")
                                .on_click(cx.listener(move |this, _, window, cx| this.new_task_dialog(project, window, cx))),
                        )
                    }),
            )
    }
}

/// gpui-cn puts a macOS-style overlay scrollbar on the shared base theme
/// (hidden between scrolls, faint thumb), and every theme rebuild, such as the
/// one for the OS appearance after the window opens, puts it back.
fn visible_scrollbars(cx: &mut App) {
    use gpui_kit::base::{ScrollbarMode, ScrollbarStyles, ScrollbarTheme, Theme};
    if cx.global::<Theme>().scrollbar.mode() == ScrollbarMode::Always {
        return;
    }
    let fg = gpui_kit::component::Theme::global(cx).muted_foreground;
    cx.global_mut::<Theme>().scrollbar = ScrollbarTheme::new()
        .with_mode(ScrollbarMode::Always)
        .with_styles(
            ScrollbarStyles::default()
                .thumb(|s| s.bg(fg.opacity(0.5)))
                .thumb_hover(|s| s.bg(fg.opacity(0.8)))
                .thumb_active(|s| s.bg(fg.opacity(0.8))),
        );
}

fn main() {
    // Run by Claude Code hooks: forward the event and exit before any UI setup.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("--hook") {
        agent::run_hook(argv.get(2).map(String::as_str).unwrap_or_default());
        return;
    }
    let mut repos = Vec::new();
    let mut agent = Some("claude".to_string());
    let mut appearance: Option<Appearance> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--agent" => agent = args.next(),
            "--no-agent" => agent = None,
            "--theme" => {
                appearance = args.next().and_then(|v| Appearance::parse(&v))
            }
            "--help" | "-h" => {
                println!("usage: ideae [--agent CMD | --no-agent] [--theme auto|light|dark] [REPO...]\nRepos join the project list, kept in ~/.local/share/ideae/state.json.");
                return;
            }
            _ => repos.push(PathBuf::from(arg)),
        }
    }
    desktop::refresh_entry();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            gpui_cn::init(cx);
            theme::init(cx);
            grammars::register();
            cx.bind_keys(gpui_cn::terminal::default_key_bindings());
            visible_scrollbars(cx);
            cx.observe_global::<gpui_kit::base::Theme>(visible_scrollbars).detach();
            // The terminal context too: a focused terminal would otherwise take them.
            for context in [None, Some(gpui_cn::terminal::KEY_CONTEXT)] {
                use task::{ShowSidePanel, ShowView};
                cx.bind_keys([
                    KeyBinding::new("ctrl-1", ShowView(0), context),
                    KeyBinding::new("ctrl-2", ShowView(1), context),
                    KeyBinding::new("ctrl-3", ShowView(2), context),
                    KeyBinding::new("ctrl-`", ShowView(3), context),
                    KeyBinding::new("alt-1", ShowSidePanel(0), context),
                    KeyBinding::new("alt-2", ShowSidePanel(1), context),
                    KeyBinding::new("alt-3", ShowSidePanel(2), context),
                    KeyBinding::new("ctrl-pagedown", task::NextTab, context),
                    KeyBinding::new("ctrl-pageup", task::PrevTab, context),
                ]);
                // JetBrains' keys: Ctrl+P and the like belong to the shell.
                cx.bind_keys([
                    KeyBinding::new("ctrl-s", task::SaveFile, None),
                    // Editor only ("Input"): in a terminal Ctrl+W deletes a word.
                    KeyBinding::new("ctrl-w", task::ExpandSelection, Some("Input")),
                    KeyBinding::new("ctrl-shift-w", task::ShrinkSelection, Some("Input")),
                    KeyBinding::new("ctrl-/", task::ToggleComment, Some("Input")),
                    KeyBinding::new("ctrl-f12", task::ShowOutline, Some("Input")),
                    // JetBrains' Go to Declaration, same as Ctrl+click.
                    KeyBinding::new("ctrl-b", gpui_kit::component::input::GoToDefinition, Some("Input")),
                    KeyBinding::new("shift-escape", task::ToggleZoom, context),
                    KeyBinding::new("ctrl-shift-n", GoToFile, context),
                    KeyBinding::new("ctrl-shift-f", FindInFiles, context),
                    KeyBinding::new("ctrl-shift-t", NewTask, context),
                ]);
            }

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1500.), px(920.)), cx)),
                app_id: Some(state::APP_ID.into()),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(repos, agent, appearance, window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
