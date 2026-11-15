//! Lists to pick from by typing: every command (Ctrl+Shift+A), recent tasks
//! and recent files (Ctrl+E, again for the other list).

use std::{path::PathBuf, rc::Rc};

use gpui_kit::component::{
    ActiveTheme as _, IndexPath, WindowExt as _,
    command::{Command, CommandItem, CommandState},
    h_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use crate::{task, workspace::Workspace};

actions!(ideae, [CommandPalette, RecentPlaces, NextWaiting]);

type Run = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Clone)]
pub struct Entry {
    label: SharedString,
    detail: SharedString,
    keys: SharedString,
    run: Run,
}

impl Entry {
    fn new(label: impl Into<SharedString>, run: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        Self { label: label.into(), detail: SharedString::default(), keys: SharedString::default(), run: Rc::new(run) }
    }

    /// Runs an action the way its keys would, from where focus is.
    fn action(label: &str, keys: &str, action: impl Action) -> Self {
        let keys = &crate::keys::label(keys);
        let action: Rc<dyn Action> = Rc::new(action);
        Self::new(label.to_string(), move |window, cx| window.dispatch_action(action.boxed_clone(), cx)).keys(keys)
    }

    fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = detail.into();
        self
    }

    fn keys(mut self, keys: &str) -> Self {
        let keys = &crate::keys::label(keys);
        self.keys = keys.to_string().into();
        self
    }
}

struct Palette {
    entries: Rc<Vec<Entry>>,
    /// Indexes into `entries` that pass the query, best first.
    shown: Rc<Vec<usize>>,
}

impl Palette {
    fn query(&mut self, query: &str, cx: &mut Context<Self>) {
        let query: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
        let shown = if query.is_empty() {
            (0..self.entries.len()).collect()
        } else {
            let mut scored: Vec<(i64, usize)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(ix, e)| {
                    let by_label = crate::finder::score(&e.label, &query);
                    let by_detail = crate::finder::score(&format!("{} {}", e.label, e.detail), &query).map(|s| s - 1000);
                    by_label.max(by_detail).map(|s| (s, ix))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            scored.into_iter().map(|(_, ix)| ix).collect()
        };
        self.shown = Rc::new(shown);
        cx.notify();
        cx.refresh_windows();
    }
}

/// The two lists Ctrl+E goes between.
#[derive(Clone, Copy, PartialEq)]
enum Recent {
    Tasks,
    Files,
}

/// Opens a list; with `recent`, which of the two it is and how to open the
/// other, which Ctrl+E and a click on its tab do.
fn open(placeholder: &str, entries: Vec<Entry>, recent: Option<(Recent, Run)>, window: &mut Window, cx: &mut App) {
    let again = recent.as_ref().map(|(_, run)| run.clone());
    let tab_shown = recent.as_ref().map(|(which, _)| *which);
    let command = cx.new(|cx| CommandState::new(window, cx));
    let palette = cx.new(|_| Palette { entries: Rc::new(entries), shown: Rc::default() });
    palette.update(cx, |p, cx| p.query("", cx));
    let placeholder: SharedString = placeholder.to_string().into();
    let focused = Rc::new(std::cell::Cell::new(false));
    window.open_dialog(cx, move |dialog, _, _| {
        let (palette, command, focused, placeholder, again) =
            (palette.clone(), command.clone(), focused.clone(), placeholder.clone(), again.clone());
        dialog.close_button(false).p_0().w(px(640.)).content(move |content, window, cx| {
            if !focused.replace(true) {
                let command = command.clone();
                window.defer(cx, move |window, cx| command.read(cx).focus_handle(cx).focus(window, cx));
            }
            let (entries, shown) = {
                let p = palette.read(cx);
                (p.entries.clone(), p.shown.clone())
            };
            let items = shown.iter().map(|&ix| item(&entries[ix]));
            let (query_owner, confirm_owner) = (palette.clone(), palette.clone());
            let again = again.clone();
            let tabs_again = again.clone();
            let mut list = Command::new(&command)
                .bordered(false)
                .filterable(false)
                .placeholder(placeholder.clone())
                .min_h(px(360.))
                .max_h(px(360.))
                .items(items);
            if let Some(tab_shown) = tab_shown {
                list = list.header(move |_, _, cx| {
                    let tab = |which: Recent, label: &'static str, cx: &App| {
                        let again = tabs_again.clone();
                        let current = which == tab_shown;
                        div()
                            .id(label)
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_sm()
                            .when(current, |d| d.bg(cx.theme().accent).text_color(cx.theme().foreground))
                            .when(!current, |d| {
                                d.text_color(cx.theme().muted_foreground).cursor_pointer().hover(|d| d.text_color(cx.theme().foreground))
                            })
                            .child(label)
                            .when(!current, |d| {
                                d.on_click(move |_, window, cx| {
                                    if let Some(again) = &again {
                                        window.close_dialog(cx);
                                        again(window, cx);
                                    }
                                })
                            })
                    };
                    h_flex()
                        .gap_1()
                        .px_2()
                        .py_1p5()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(tab(Recent::Tasks, "Recent tasks", cx))
                        .child(tab(Recent::Files, "Recent files", cx))
                        .child(div().flex_1())
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!("{} switches", crate::keys::label("Ctrl+E"))))
                });
            }
            content.child(
                div()
                    .on_action(move |_: &RecentPlaces, window, cx| {
                        if let Some(again) = &again {
                            window.close_dialog(cx);
                            again(window, cx);
                        }
                    })
                    .child(
                        list.on_query(move |query, _, cx| {
                                let _ = query_owner.update(cx, |p, cx| p.query(query, cx));
                            })
                            .on_confirm(move |index: IndexPath, window, cx| {
                                let run = {
                                    let p = confirm_owner.read(cx);
                                    p.shown.get(index.row).map(|&ix| p.entries[ix].run.clone())
                                };
                                if let Some(run) = run {
                                    window.close_dialog(cx);
                                    run(window, cx);
                                }
                            }),
                    ),
            )
        })
    });
}

fn item(entry: &Entry) -> CommandItem {
    let entry = entry.clone();
    CommandItem::new().label(entry.label.clone()).child(move |_, cx| {
        h_flex()
            .gap_3()
            .min_w_0()
            .w_full()
            .child(div().flex_none().child(entry.label.clone()))
            .child(div().flex_1().min_w_0().truncate().text_xs().text_color(cx.theme().muted_foreground).child(entry.detail.clone()))
            .child(div().flex_none().text_xs().text_color(cx.theme().muted_foreground).child(entry.keys.clone()))
            .into_any_element()
    })
}

impl Workspace {
    pub fn command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::{FindInFiles, GoToFile, NewTask, OpenSettings, Quit};
        use task::{NavigateBack, NavigateForward, NextTab, PrevTab, ShowOutline, ShowSidePanel, ShowView, ToggleZoom};
        let this = cx.weak_entity();
        let with_task = |f: fn(&mut task::TaskView, &mut Window, &mut Context<task::TaskView>)| {
            let this = this.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some(t) = this.upgrade().and_then(|w| w.read(cx).active_view()) {
                    t.update(cx, |t, cx| f(t, window, cx));
                }
            }
        };
        let w = this.clone();
        let w2 = this.clone();
        let w3 = this.clone();
        let w4 = this.clone();
        let w5 = this.clone();
        let entries = vec![
            Entry::action("New task…", "Ctrl+Shift+T", NewTask),
            Entry::action("Go to file…", "Ctrl+Shift+N", GoToFile),
            Entry::action("Find in files…", "Ctrl+Shift+F", FindInFiles),
            Entry::action("Find in terminal…", "Ctrl+F", task::FindInTerminal).detail("the scrollback"),
            Entry::action("Next waiting task", "Alt+Down", NextWaiting).detail("an agent that asks or is done"),
            Entry::new("Recent tasks", move |window, cx| {
                let _ = w.update(cx, |w, cx| w.recent_tasks(window, cx));
            })
            .keys("Ctrl+E"),
            Entry::new("Recent files", move |window, cx| {
                let _ = w2.update(cx, |w, cx| w.recent_files(window, cx));
            })
            .keys("Ctrl+E Ctrl+E"),
            Entry::action("Agent", "Ctrl+1", ShowView(0)),
            Entry::action("Changes", "Ctrl+2", ShowView(1)).detail("the diff"),
            Entry::action("History", "Ctrl+3", ShowView(2)).detail("the branch's commits"),
            Entry::action("Shell", "Ctrl+`", ShowView(3)).detail("terminal"),
            Entry::new("New terminal", with_task(task::TaskView::new_terminal)),
            Entry::new("New browser tab", with_task(task::TaskView::new_browser)).detail("web"),
            Entry::new("Restart agent", with_task(task::TaskView::restart_agent)),
            Entry::action("Changes panel", "Alt+1", ShowSidePanel(0)),
            Entry::action("Files panel", "Alt+2", ShowSidePanel(1)).detail("project tree"),
            Entry::action("Structure panel", "Alt+3", ShowSidePanel(2)),
            Entry::action("File structure", "Ctrl+F12", ShowOutline).detail("declarations of the open file"),
            Entry::action("Next tab", "Ctrl+PgDn", NextTab),
            Entry::action("Previous tab", "Ctrl+PgUp", PrevTab),
            Entry::action("Back", "Alt+Left", NavigateBack).detail("navigate"),
            Entry::action("Forward", "Alt+Right", NavigateForward).detail("navigate"),
            Entry::action("Focus mode", "Shift+Esc", ToggleZoom).detail("zoom the pane"),
            Entry::new("Add project…", move |window, cx| {
                let _ = w3.update(cx, |w, cx| w.add_project_dialog(window, cx));
            }),
            Entry::new("Clean up merged tasks…", move |window, cx| {
                let _ = w4.update(cx, |w, cx| {
                    if let Some(project) = w.active_project() {
                        w.clean_up_merged(project, window, cx);
                    }
                });
            }),
            Entry::new("Language servers on/off", move |_, cx| {
                let _ = w5.update(cx, |w, cx| w.toggle_power_saving(cx));
            })
            .detail("power saving"),
            Entry::action("Settings", "Ctrl+,", OpenSettings),
            Entry::action("Quit", "Ctrl+Q", Quit),
        ];
        open("Run a command…", entries, None, window, cx);
    }

    /// Tasks by when you were last in them, the current one left out.
    pub fn recent_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut tasks: Vec<(u64, PathBuf, String, String)> = Vec::new();
        for (pi, p) in self.projects.iter().enumerate() {
            for (ti, t) in p.tasks.iter().enumerate() {
                if self.active.as_deref() == Some(t.path.as_path()) || t.busy.is_some() {
                    continue;
                }
                let when = crate::sidebar::ago(t.last_active);
                let detail = if when.is_empty() { p.name.clone() } else { format!("{} · {when}", p.name) };
                tasks.push((t.last_active, t.path.clone(), self.task_title(pi, ti, cx), detail));
            }
        }
        tasks.sort_by(|a, b| b.0.cmp(&a.0));
        let this = cx.weak_entity();
        let entries = tasks
            .into_iter()
            .map(|(_, path, title, detail)| {
                let this = this.clone();
                Entry::new(title, move |window, cx| {
                    let _ = this.update(cx, |w, cx| {
                        w.switch(&path, window, cx);
                        if let Some(view) = w.active_view() {
                            view.update(cx, |t, cx| t.show_view(0, window, cx));
                        }
                    });
                })
                .detail(detail)
            })
            .collect();
        let this = cx.weak_entity();
        let again: Run = Rc::new(move |window, cx| {
            let _ = this.update(cx, |w, cx| w.recent_files(window, cx));
        });
        open("Recent tasks…", entries, Some((Recent::Tasks, again)), window, cx);
    }

    /// Files of the current task by when you were last in them.
    pub fn recent_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_view() else { return };
        let (root, files) = {
            let t = view.read(cx);
            (t.root.clone(), t.recent_files(cx))
        };
        let task = view.downgrade();
        let entries = files
            .into_iter()
            .map(|path| {
                let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
                let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let dir = rel.parent().map(|d| d.display().to_string()).unwrap_or_default();
                let task = task.clone();
                Entry::new(name, move |window, cx| {
                    let path = path.clone();
                    let _ = task.update(cx, |t, cx| t.reopen_recent(path, window, cx));
                })
                .detail(dir)
            })
            .collect();
        let this = cx.weak_entity();
        let again: Run = Rc::new(move |window, cx| {
            let _ = this.update(cx, |w, cx| w.recent_tasks(window, cx));
        });
        open("Recent files…", entries, Some((Recent::Files, again)), window, cx);
    }
}
