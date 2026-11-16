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

    /// Runs a shortcut's action the way its keys would, from where focus is,
    /// with its keys as Settings has them.
    fn shortcut(label: &str, id: &str) -> Self {
        let shortcut = crate::keymap::find(id).expect("a shortcut in keymap.rs");
        let action: Rc<dyn Action> = Rc::from(shortcut.action());
        let mut entry = Self::new(label.to_string(), move |window, cx| window.dispatch_action(action.boxed_clone(), cx));
        entry.keys = crate::keymap::label(id).into();
        entry
    }

    fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = detail.into();
        self
    }

    fn keys(mut self, keys: String) -> Self {
        self.keys = keys.into();
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
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!("{} switches", crate::keymap::label("recent"))))
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
        let this = cx.weak_entity();
        let with_task = |f: fn(&mut task::TaskView, &mut Window, &mut Context<task::TaskView>)| {
            let this = this.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some(t) = this.upgrade().and_then(|w| w.read(cx).active_view()) {
                    t.update(cx, |t, cx| f(t, window, cx));
                }
            }
        };
        // A command of the workspace, with the word that says what kind it is.
        let own = |label: &str, kind: &str, run: fn(&mut Workspace, &mut Window, &mut Context<Workspace>)| {
            let this = this.clone();
            Entry::new(label.to_string(), move |window, cx| {
                let _ = this.update(cx, |w, cx| run(w, window, cx));
            })
            .detail(kind.to_string())
        };
        let key = |entry: Entry, id: &str| entry.keys(crate::keymap::label(id));
        let keyed = |label: &str, id: &str, kind: &str| Entry::shortcut(label, id).detail(kind.to_string());
        let (presenting, saving) = (self.is_presenting(), crate::lsp::POWER_SAVING.load(std::sync::atomic::Ordering::Relaxed));
        // In the order of the menu: what to start, where to go, the task's
        // views, git, the editor, the window, the app.
        // The spaces to go to, when there are some.
        let mut to_spaces: Vec<Entry> = Vec::new();
        if !self.spaces.is_empty() {
            let go = |label: String, to: Option<String>| {
                let this = this.clone();
                Entry::new(label, move |window, cx| {
                    let to = to.clone();
                    let _ = this.update(cx, |w, cx| w.switch_space(to, window, cx));
                })
                .detail("space: the projects in the window")
            };
            to_spaces.push(go("All spaces".into(), None));
            to_spaces.extend(self.spaces.iter().map(|space| go(format!("Space: {}", space.name), Some(space.name.clone()))));
        }
        to_spaces.push(own("Spaces…", "space: add, name, remove", |w, window, cx| w.spaces_dialog(window, cx)));
        let mut entries = vec![
            keyed("New task…", "new_task", "task"),
            keyed("Next waiting task", "next_waiting", "task: an agent that asks or is done"),
            key(own("Recent tasks", "task", |w, window, cx| w.recent_tasks(window, cx)), "recent"),
            own("Recent files", "go to", |w, window, cx| w.recent_files(window, cx)).keys({
                let e = crate::keymap::label("recent");
                if e.is_empty() { e } else { format!("{e} {e}") }
            }),
            own("Add project…", "task", |w, window, cx| w.add_project_dialog(window, cx)),
            own("Clean up merged tasks…", "task", |w, window, cx| {
                if let Some(project) = w.active_project() {
                    w.clean_up_merged(project, window, cx);
                }
            }),
            keyed("Go to file…", "go_to_file", "go to"),
            keyed("Go to class or symbol…", "go_to_symbol", "go to: anywhere in the project"),
            keyed("Find in files…", "find_in_files", "go to"),
            keyed("Find in terminal…", "find_in_terminal", "go to: the scrollback"),
            keyed("Go to line…", "go_to_line", "go to"),
            keyed("File structure", "outline", "go to: declarations of the open file"),
            keyed("Go to definition", "definition", "go to"),
            keyed("Implementations and usages", "usages", "go to"),
            keyed("Back", "back", "go to"),
            keyed("Forward", "forward", "go to"),
            keyed("Agent", "agent", "view"),
            keyed("Changes", "changes", "view: the diff"),
            keyed("History", "history", "view: the branch's commits"),
            keyed("Shell", "shell", "view: terminal"),
            Entry::new("New terminal", with_task(task::TaskView::new_terminal)).detail("view"),
            Entry::new("New browser tab", with_task(task::TaskView::new_browser)).detail("view: web"),
            Entry::new("Restart agent", with_task(task::TaskView::restart_agent)).detail("view"),
            keyed("Next tab", "next_tab", "view"),
            keyed("Previous tab", "prev_tab", "view"),
            keyed("Focus mode", "focus_mode", "view: zoom the pane"),
            keyed("Changes panel", "changes_panel", "panel"),
            keyed("Files panel", "files_panel", "panel: project tree"),
            keyed("Structure panel", "structure_panel", "panel"),
            keyed("Run panel", "run_panel", "panel: services, dev servers, workers"),
            keyed("Branches…", "branches", "git: open as a task, compare, delete"),
            Entry::new("Compare with a branch, tag or commit…", with_task(|t, window, cx| t.compare_with_dialog(None, window, cx))).detail("git: the working tree"),
            Entry::new("Compare this file with…", with_task(|t, window, cx| t.compare_open_file(window, cx))).detail("git: the file in the editor"),
            keyed("Complete here", "complete", "editor"),
            keyed("Quick fixes", "quick_fix", "editor: what the language server offers"),
            keyed("Rename…", "rename", "editor: the symbol, everywhere"),
            keyed("Reformat file", "format", "editor"),
            keyed("Parameter info", "parameter_info", "editor: the call being written"),
            keyed("Fold", "fold", "editor"),
            keyed("Unfold", "unfold", "editor"),
            keyed("Fold all", "fold_all", "editor"),
            keyed("Unfold all", "unfold_all", "editor"),
            key(own("Theme…", "window: each is drawn as you go through them", |w, window, cx| w.theme_picker(window, cx)), "theme"),
            own("Light theme", "window", |w, window, cx| w.set_appearance(crate::theme::Appearance::Light, window, cx)),
            own("Dark theme", "window", |w, window, cx| w.set_appearance(crate::theme::Appearance::Dark, window, cx)),
            own("Theme as the system", "window: light by day, dark by night", |w, window, cx| w.set_appearance(crate::theme::Appearance::Auto, window, cx)),
            own("Text of its usual size", "window: editor, diff and terminal", |_, _, cx| crate::zoom::reset_all(cx)),
            own(if presenting { "End presentation" } else { "Enter presentation mode…" }, "window: only the tasks chosen", |w, window, cx| {
                if w.is_presenting() { w.end_presentation(window, cx) } else { w.presentation_dialog(window, cx) }
            }),
            own(if saving { "Leave power saving" } else { "Save power" }, "window: language servers and background work", |w, _, cx| w.toggle_power_saving(cx)),
            keyed("Settings…", "settings", "app"),
            own("Keyboard shortcuts…", "app", |w, window, cx| w.open_settings_at(crate::settings_ui::KEYS_PAGE, window, cx)),
            own("Restart", "app", |w, window, cx| w.restart(window, cx)),
            keyed("Quit", "quit", "app"),
        ];
        // With the window's own: before the app's.
        let app = entries.iter().position(|entry| entry.label.as_ref() == "Settings…").unwrap_or(entries.len());
        entries.splice(app..app, to_spaces);
        open("Run a command…", entries, None, window, cx);
    }

    /// Tasks by when you were last in them, the current one left out.
    pub fn recent_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut tasks: Vec<(u64, PathBuf, String, String)> = Vec::new();
        for (pi, p) in self.projects.iter().enumerate() {
            for (ti, t) in p.tasks.iter().enumerate() {
                if self.active.as_deref() == Some(t.path.as_path()) || t.busy.is_some() || !self.shows(pi, ti) {
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
