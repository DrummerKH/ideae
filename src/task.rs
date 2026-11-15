use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gpui_cn::terminal::{
    LocalTerminalOptions, Terminal, TerminalConfig, TerminalEvent, TerminalState, WorkingDirectory,
};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, StyledExt as _, h_flex,
    scroll::ScrollableElement as _,
    button::{Button, ButtonVariants as _},
    diff::{Diff, DiffEvent, DiffFile, DiffFileStatus, DiffLineRange, DiffMode, DiffState},
    input::{DefinitionProvider, Editor, EditorState, HoverProvider, InputEvent, Position, Rope, RopeExt as _},
    resizable::{h_resizable, resizable_panel},
    tab::{Tab, TabBar},
    list::ListItem,
    tree::{TreeEvent, TreeItem, TreeState, tree},
    Disableable as _, IconName, Selectable as _, WindowExt as _,
    menu::{ContextMenuExt as _, DropdownMenu as _},
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use notify::{RecursiveMode, Watcher as _};

use crate::{git, history::HistoryView, lsp};

// ---------------------------------------------------------------- terminal

pub struct TermView {
    id: SharedString,
    pub state: Entity<TerminalState>,
}

impl TermView {
    fn new(id: SharedString, state: Entity<TerminalState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self { id, state }
    }
}

impl Render for TermView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let highlight = cx.theme().primary.opacity(0.15);
        div()
            .id("terminal-drop")
            .size_full()
            // Files dropped from a file manager: their paths, typed in as text.
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(highlight))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                cx.stop_propagation();
                let text = paths.paths().iter().map(|p| shell_quote(&p.display().to_string())).collect::<Vec<_>>().join(" ");
                this.state.update(cx, |s, cx| s.paste_text(format!("{text} "), cx));
                this.state.read(cx).focus_handle().clone().focus(window, cx);
            }))
            .child(Terminal::new(self.id.clone(), &self.state))
    }
}

/// Quoted for a shell only when it needs to be.
fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:@,%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

// ---------------------------------------------------------------- file

actions!(ideae_file, [SaveFile, ExpandSelection, ShrinkSelection, ToggleComment, ShowOutline, SendToAgent]);
actions!(ideae_task, [ToggleZoom, SendDiffToAgent]);

actions!(ideae_task, [NextTab, PrevTab]);

/// Ctrl+1/2/3/`: the agent, Changes, History, a shell.
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(namespace = ideae_task, no_json)]
pub struct ShowView(pub usize);

/// Alt+1/2/3: the side panel's Changes, Files, Structure.
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(namespace = ideae_task, no_json)]
pub struct ShowSidePanel(pub usize);

pub struct FileView {
    path: PathBuf,
    editor: Entity<EditorState>,
    /// The text as last read from or written to disk, and that file's mtime.
    saved: SharedString,
    disk_mtime: Option<std::time::SystemTime>,
    pub dirty: bool,
    /// Changed on disk while it had unsaved edits: the user picks a side.
    conflict: bool,
    /// The last save failed, with why.
    error: Option<String>,
    lsp_version: i32,
    /// Bumped on every change of the text, for caches of things derived from it.
    pub edit_version: u64,
    _lsp_sync: Task<()>,
    /// The task's worktree (for git) and the task, to open a blamed commit.
    root: PathBuf,
    task: WeakEntity<TaskView>,
    /// Who last changed each line, and the line the cursor is on.
    blame: Rc<Vec<Option<git::BlameLine>>>,
    cursor_line: u32,
    _blame: Task<()>,
    /// Selections Ctrl+W grew from, for Ctrl+Shift+W to step back through.
    expand_stack: Vec<std::ops::Range<usize>>,
    last_expanded: Option<std::ops::Range<usize>>,
    /// What the language server is doing for this file, shown in the header.
    lsp_status: SharedString,
    /// None while starting, then whether it works.
    lsp_ok: Option<bool>,
    lsp_client: Option<Arc<lsp::LspClient>>,
    lsp_program: String,
    _poll: Task<()>,
}

fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl FileView {
    fn on_edit(&mut self, cx: &mut Context<Self>) {
        self.edit_version += 1;
        let text = self.editor.read(cx).value();
        let dirty = text != self.saved;
        if dirty != self.dirty {
            self.dirty = dirty;
            cx.notify();
        }
        // Tell the language server after typing pauses, so hover and
        // definitions see the edited text.
        let Some(client) = self.lsp_client.clone() else { return };
        self.lsp_version += 1;
        let (path, version) = (self.path.clone(), self.lsp_version);
        self._lsp_sync = cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            cx.background_spawn(async move {
                let _ = client.did_change(&path, version, &text);
            })
            .await;
        });
    }

    /// Saves edits unless the file changed on disk meanwhile: then the
    /// conflict bar asks, rather than silently overwriting the other side.
    pub fn autosave(&mut self, cx: &mut Context<Self>) {
        if self.dirty && !self.conflict {
            self.save(cx);
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value();
        match std::fs::write(&self.path, text.as_bytes()) {
            Ok(()) => {
                self.saved = text;
                self.disk_mtime = mtime(&self.path);
                self.dirty = false;
                self.conflict = false;
                self.error = None;
                self.load_blame(cx);
                if let Some(client) = self.lsp_client.clone() {
                    let path = self.path.clone();
                    cx.background_spawn(async move {
                        let _ = client.did_save(&path);
                    })
                    .detach();
                }
            }
            Err(e) => self.error = Some(format!("Save failed: {e}")),
        }
        cx.notify();
    }

    /// Called on every worktree refresh: picks up changes made on disk (by the
    /// agent, git, another editor). Clean buffers reload in place; edited ones
    /// get a choice instead of losing either side.
    pub fn check_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = mtime(&self.path);
        if now == self.disk_mtime || now.is_none() {
            return;
        }
        let Ok(content) = std::fs::read_to_string(&self.path) else { return };
        self.disk_mtime = now;
        if content.as_str() == self.saved.as_ref() {
            return;
        }
        if self.dirty {
            self.conflict = true;
            cx.notify();
        } else {
            self.reload(content, window, cx);
        }
    }

    fn reload(&mut self, content: String, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_version += 1;
        self.saved = content.clone().into();
        self.conflict = false;
        let content_for_lsp = content.clone();
        self.editor.update(cx, |state, cx| {
            let cursor = state.cursor_position();
            state.set_value(content, window, cx);
            state.set_cursor_position(cursor, window, cx);
        });
        self.dirty = false;
        self.load_blame(cx);
        if let Some(client) = self.lsp_client.clone() {
            self.lsp_version += 1;
            let (path, version) = (self.path.clone(), self.lsp_version);
            cx.background_spawn(async move {
                let _ = client.did_change(&path, version, &content_for_lsp);
            })
            .detach();
        }
        cx.notify();
    }

    fn reload_from_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(content) = std::fs::read_to_string(&self.path) {
            self.disk_mtime = mtime(&self.path);
            self.reload(content, window, cx);
        }
    }

    pub fn load_blame(&mut self, cx: &mut Context<Self>) {
        let (root, path) = (self.root.clone(), self.path.clone());
        self._blame = cx.spawn(async move |this, cx| {
            let blame = cx.background_spawn(async move { git::blame(&root, &path) }).await;
            let _ = this.update(cx, |this, cx| {
                this.blame = Rc::new(blame);
                cx.notify();
            });
        });
    }

    fn render_blame(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.dirty {
            return None;
        }
        let info = self.blame.get(self.cursor_line as usize)?.clone()?;
        let text = if info.uncommitted() {
            "not committed yet".to_string()
        } else {
            format!("{}, {} · {} · {}", info.author, git::ago(info.time), info.summary, &info.hash[..8])
        };
        let task = self.task.clone();
        let hash = info.hash.clone();
        let uncommitted = info.uncommitted();
        Some(
            div()
                .id("blame")
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(cx.theme().muted_foreground.opacity(0.8))
                .when(!uncommitted, |this| {
                    this.cursor_pointer()
                        .hover(|s| s.text_color(cx.theme().foreground))
                        .on_click(move |_, window, cx| {
                            let _ = task.update(cx, |t, cx| t.show_commit(&hash, window, cx));
                        })
                })
                .child(text)
                .into_any_element(),
        )
    }

    /// Ctrl+W: select the smallest syntax node larger than the selection (a
    /// word, an expression, a statement, a block…); without a grammar, the
    /// line, then the whole text.
    fn expand_selection(&mut self, cx: &mut Context<Self>) {
        let (text, range) = {
            let state = self.editor.read(cx);
            (state.text().to_string(), state.selected_range())
        };
        // A selection moved by hand since the last step starts a new chain.
        if self.expand_stack.last().is_some_and(|_| self.last_expanded.as_ref() != Some(&range)) {
            self.expand_stack.clear();
        }
        let next = syntax_parent(&self.path, &text, range.clone()).unwrap_or_else(|| plain_parent(&text, range.clone()));
        if next == range {
            return;
        }
        self.expand_stack.push(range);
        self.last_expanded = Some(next.clone());
        self.editor.update(cx, |state, cx| state.set_selected_range(next, cx));
    }

    /// Ctrl+Shift+W: back to the selection before the last Ctrl+W.
    fn shrink_selection(&mut self, cx: &mut Context<Self>) {
        let Some(previous) = self.expand_stack.pop() else { return };
        self.last_expanded = Some(previous.clone());
        self.editor.update(cx, |state, cx| state.set_selected_range(previous, cx));
    }

    /// Ctrl+/: comment the selected lines out, or back in when all are.
    fn toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, range) = {
            let state = self.editor.read(cx);
            (state.text().to_string(), state.selected_range())
        };
        let Some((start, end, replacement)) = toggle_comment_lines(&self.path, &text, range) else { return };
        self.editor.update(cx, |state, cx| {
            state.set_selected_range(start..end, cx);
            state.replace(replacement.clone(), window, cx);
            state.set_selected_range(start..start + replacement.len(), cx);
        });
    }

    fn go_to_line(&mut self, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            state.set_cursor_position(Position::new(line, 0), window, cx);
        });
        self.editor.read(cx).focus_handle(cx).focus(window, cx);
    }
}

impl FileView {
    fn is_busy(&self) -> bool {
        self.lsp_client.as_ref().is_some_and(|c| c.busy().is_some())
    }

    fn status_text(&self) -> String {
        let busy = self.lsp_client.as_ref().and_then(|c| c.busy());
        match (self.lsp_program.is_empty(), self.lsp_ok, busy) {
            (true, _, _) => self.lsp_status.to_string(),
            (false, Some(true), Some(work)) => format!("{}: {work}…", self.lsp_program),
            (false, _, _) => format!("{}: {}", self.lsp_program, self.lsp_status),
        }
    }
}

impl Render for FileView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let warning = hsla(0.08, 0.9, 0.55, 1.);
        v_flex()
            .size_full()
            .on_action(cx.listener(|this, _: &SaveFile, _, cx| this.save(cx)))
            .on_action(cx.listener(|this, _: &ExpandSelection, _, cx| this.expand_selection(cx)))
            .on_action(cx.listener(|this, _: &ShrinkSelection, _, cx| this.shrink_selection(cx)))
            .on_action(cx.listener(|this, _: &ToggleComment, window, cx| this.toggle_comment(window, cx)))
            .child(
                h_flex()
                    .justify_between()
                    .gap_4()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_none().max_w(relative(0.45)).truncate().child(self.path.display().to_string()))
                    .children(self.render_blame(cx))
                    .when(!crate::ide::installed().is_empty(), |header| {
                        let path = self.path.clone();
                        let line = self.cursor_line + 1;
                        header.child(
                            Button::new("open-in")
                                .xsmall()
                                .ghost()
                                .label("Open in ▾")
                                .dropdown_menu(move |mut menu, _, _| {
                                    for ide in crate::ide::installed() {
                                        let (ide, path) = (ide.clone(), path.clone());
                                        menu = menu.item(
                                            gpui_kit::component::menu::PopupMenuItem::new(ide.name)
                                                .on_click(move |_, _, _| ide.open(&path, line)),
                                        );
                                    }
                                    menu
                                }),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .when(self.lsp_ok == Some(true) && !self.is_busy(), |s| s.text_color(gpui_kit::green()))
                            .when(self.lsp_ok == Some(false) && self.lsp_status.contains("failed"), |s| {
                                s.text_color(gpui_kit::red())
                            })
                            .child(self.status_text()),
                    ),
            )
            .when(self.conflict, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .bg(warning.opacity(0.15))
                        .child(div().flex_1().child("Changed on disk while you have unsaved edits."))
                        .child(
                            Button::new("reload")
                                .xsmall()
                                .ghost()
                                .label("Load disk version")
                                .on_click(cx.listener(|this, _, window, cx| this.reload_from_disk(window, cx))),
                        )
                        .child(
                            Button::new("keep")
                                .xsmall()
                                .ghost()
                                .label("Keep mine")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.conflict = false;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(div().px_3().py_1().text_xs().text_color(gpui_kit::red()).child(error))
            })
            .child(
                div().flex_1().min_h_0().child(
                    Editor::new(&self.editor)
                        // Replaces the built-in menu, so its items are listed again.
                        .context_menu(|menu, _, _| {
                            use gpui_kit::component::input::{Copy, Cut, GoToDefinition, Paste, SelectAll};
                            menu.menu("Go to Definition", Box::new(GoToDefinition))
                                .separator()
                                .menu("Cut", Box::new(Cut))
                                .menu("Copy", Box::new(Copy))
                                .menu("Paste", Box::new(Paste))
                                .menu("Select All", Box::new(SelectAll))
                                .separator()
                                .menu("Comment / Uncomment", Box::new(ToggleComment))
                                .menu("File Structure", Box::new(ShowOutline))
                                .separator()
                                .menu("Send to Agent", Box::new(SendToAgent))
                        })
                        .bordered(false)
                        .p_0()
                        .h_full()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(cx.theme().mono_font_size),
                ),
            )
    }
}

/// Bridges the editor's hover and definition hooks to a language server.
struct LspBridge {
    client: Arc<lsp::LspClient>,
    uri: String,
}

impl HoverProvider for LspBridge {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Option<lsp_types::Hover>>> {
        let pos = text.offset_to_position(offset);
        if std::env::var_os("IDEAE_DEBUG").is_some() {
            eprintln!("hover requested at {pos:?}");
        }
        let (client, uri) = (self.client.clone(), self.uri.clone());
        cx.background_spawn(async move {
            let result = client.hover(&uri, pos).inspect_err(|e| eprintln!("hover: {e:#}"));
            if std::env::var_os("IDEAE_DEBUG").is_some() {
                eprintln!("hover {uri} {pos:?} -> {}", if matches!(result, Ok(Some(_))) { "some" } else { "none/err" });
            }
            result
        })
    }
}

impl DefinitionProvider for LspBridge {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<lsp_types::LocationLink>>> {
        let pos = text.offset_to_position(offset);
        let (client, uri) = (self.client.clone(), self.uri.clone());
        cx.background_spawn(async move {
            let result = client.definition(&uri, pos).inspect_err(|e| eprintln!("definition: {e:#}"));
            if std::env::var_os("IDEAE_DEBUG").is_some() {
                eprintln!("definition {uri} {pos:?} -> {result:?}");
            }
            result
        })
    }
}

// ---------------------------------------------------------------- changes

#[derive(Clone, Copy, PartialEq)]
pub enum DiffScope {
    /// The worktree against HEAD.
    Uncommitted,
    /// The worktree against where the branch forked from its base.
    Branch,
}

pub struct ChangesView {
    root: PathBuf,
    pub diff: Entity<DiffState>,
    scope: DiffScope,
    /// File counts for both scopes, and the base branch, for the toggle.
    counts: (usize, usize),
    base: Option<String>,
    /// First changed line per file in the diff shown, for "open".
    first_lines: Rc<HashMap<String, u32>>,
    selection: Option<DiffLineRange>,
    task: WeakEntity<TaskView>,
}

impl ChangesView {
    fn new(root: PathBuf, task: WeakEntity<TaskView>, cx: &mut Context<Self>) -> Self {
        let diff = cx.new(|cx| DiffState::new([], cx).with_mode(DiffMode::Split));
        cx.observe(&diff, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&diff, |this, _, event: &DiffEvent, cx| match event {
            DiffEvent::SelectionEnded(range) => {
                this.selection = Some(range.clone());
                cx.notify();
            }
            // Not cleared on SelectionChanged(None): pressing the Open button
            // clears the diff selection, which would remove the button mid-click.
            _ => {}
        })
        .detach();
        Self { root, diff, scope: DiffScope::Uncommitted, counts: (0, 0), base: None, first_lines: Rc::default(), selection: None, task }
    }

    /// The lines selected by their numbers, to the agent with their location.
    fn send_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.diff.read(cx).selected_lines().or(self.selection.clone()) else { return };
        let code = self.diff.read(cx).selected_text(cx);
        if code.is_empty() {
            return;
        }
        let location = if range.start() == range.end() {
            format!("{}:{}", range.path(), range.start())
        } else {
            format!("{}:{}-{}", range.path(), range.start(), range.end())
        };
        let language = lsp::language_for(std::path::Path::new(range.path().as_ref()));
        let _ = self.task.update(cx, |t, cx| t.send_to_agent(location, code, language, window, cx));
    }

    fn set_scope(&mut self, scope: DiffScope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        // Read from the task rather than asking it to push: it would read this
        // view back while it is being updated.
        let Ok((files, lines)) = self.task.read_with(cx, |task, _| match scope {
            DiffScope::Uncommitted => (task.uncommitted_diff.clone(), git::first_changed_lines(&task.patch)),
            DiffScope::Branch => (task.branch_diff.clone(), git::first_changed_lines(&task.branch_patch)),
        }) else {
            return;
        };
        self.first_lines = Rc::new(lines);
        self.diff.update(cx, |d, cx| d.set_files(files, cx));
        cx.notify();
    }

    fn open_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.selection.clone() else { return };
        let path = self.root.join(range.path().as_ref());
        let line = range.start().saturating_sub(1) as u32;
        let _ = self.task.update(cx, |task, cx| task.open_file(path, line, window, cx));
    }
}

impl Render for ChangesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.diff.read(cx).mode();
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
                        let (root, task, view) = (self.root.clone(), self.task.clone(), cx.weak_entity());
                        Button::new("pick-base")
                            .small()
                            .ghost()
                            .label("▾")
                            .tooltip("Compare with another branch")
                            .dropdown_menu(move |mut menu, _, _| {
                                for branch in git::branches(&root).into_iter().take(40) {
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
                    .child(
                        Button::new("mode")
                            .small()
                            .ghost()
                            .label(if mode == DiffMode::Split { "Split" } else { "Unified" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.diff.update(cx, |d, cx| {
                                    let next = if d.mode() == DiffMode::Split {
                                        DiffMode::Unified
                                    } else {
                                        DiffMode::Split
                                    };
                                    d.set_mode(next, cx);
                                });
                            })),
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
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Ctrl+click a line to open the file there"),
                    ),
            )
            .child(div().id("changes-diff").flex_1().min_h_0()
                .on_action(cx.listener(|this, _: &SendDiffToAgent, window, cx| this.send_selection(window, cx)))
                .context_menu(|menu, _, _| menu.menu("Send selected lines to Agent", Box::new(SendDiffToAgent)))
                .map(|this| {
                if empty {
                    this.flex()
                        .items_center()
                        .justify_center()
                        .text_color(cx.theme().muted_foreground)
                        .child("No uncommitted changes")
                } else {
                    this.child(
                        Diff::new(&self.diff)
                            .line_number(true)
                            .render_header_suffix({
                                let (root, task, lines) = (self.root.clone(), self.task.clone(), self.first_lines.clone());
                                move |file, _, _| open_file_button(&root, &task, file, &lines)
                            })
                            .on_line_click({
                                let view = cx.weak_entity();
                                move |position, event, window, cx| {
                                    // Ctrl+click opens; a plain click stays in the review.
                                    if !event.modifiers().secondary(cx) {
                                        return;
                                    }
                                    let path = position.path().clone();
                                    let line = position.line().saturating_sub(1) as u32;
                                    let _ = view.update(cx, |this, cx| {
                                        let path = this.root.join(path.as_ref());
                                        let _ = this.task.update(cx, |task, cx| {
                                            task.open_file(path, line, window, cx)
                                        });
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

// ---------------------------------------------------------------- task

enum TabKind {
    Terminal(Entity<TermView>),
    Changes,
    History,
    File(Entity<FileView>),
    Browser(Entity<crate::browser::BrowserView>),
    /// A file that cannot be shown as text: binary, or unreadable.
    Binary(Entity<BinaryView>),
}

pub struct BinaryView {
    path: PathBuf,
    reason: String,
}

impl Render for BinaryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.path.clone();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(div().text_lg().child(
                self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            ))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(self.reason.clone()))
            .child(
                Button::new("open-system")
                    .small()
                    .outline()
                    .label("Open in the system app")
                    .on_click(move |_, _, cx| cx.open_with_system(&path)),
            )
    }
}

/// Set when a tab drag starts. Pane dividers drag too (gpui's drag-and-drop
/// does not say what is dragged), and those must not show the drop zones.
static TAB_DRAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn tab_drag_active(cx: &App) -> bool {
    if !cx.has_active_drag() {
        TAB_DRAG.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    cx.has_active_drag() && TAB_DRAG.load(std::sync::atomic::Ordering::Relaxed)
}

/// A tab being dragged to the other pane; also its drag preview.
#[derive(Clone)]
struct DraggedTab {
    tab_id: usize,
    title: SharedString,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .text_sm()
            .rounded_md()
            .bg(cx.theme().secondary)
            .border_1()
            .border_color(cx.theme().primary)
            .child(self.title.clone())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum RightPane {
    Changes,
    Files,
    /// The declarations of the file last in view.
    Structure,
}

struct TabItem {
    /// Stable across inserts and removals, unlike the index.
    id: usize,
    title: SharedString,
    kind: TabKind,
    /// In the right pane; every tab is in the left one when not split.
    right: bool,
}

impl TabItem {
    fn new(title: impl Into<SharedString>, kind: TabKind) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        Self { id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), title: title.into(), kind, right: false }
    }
}

struct ChangedFile {
    path: SharedString,
    first_line: u32,
    status: DiffFileStatus,
    additions: usize,
    deletions: usize,
}

pub enum TaskEvent {
    Changed,
}

impl EventEmitter<TaskEvent> for TaskView {}

pub struct TaskView {
    pub root: PathBuf,
    pub branch: SharedString,
    pub agent_status: crate::agent::AgentStatus,
    hook_socket: Option<PathBuf>,
    /// Numbers terminals so each has a unique element id and default title.
    terminal_seq: usize,
    /// Status changed to one that wants attention while the task was not in view.
    pub unseen: bool,
    tabs: Vec<TabItem>,
    /// The tab shown in each pane, by id. `right_active` is None unless split.
    left_active: usize,
    right_active: Option<usize>,
    /// The pane you work in: new tabs open there.
    focus_right: bool,
    /// Focus mode: this pane (right or not) fills the window alone.
    zoomed: Option<bool>,
    last_tab_click: Option<(usize, std::time::Instant)>,
    /// Held when the tab in view takes no focus of its own (Changes, History,
    /// a placeholder), so keys such as Shift+Esc still reach the task.
    focus: FocusHandle,
    changes: Entity<ChangesView>,
    history: Entity<HistoryView>,
    files: Vec<ChangedFile>,
    patch: String,
    /// Parsed diffs of both scopes; the Changes tab shows one of them.
    uncommitted_diff: Vec<DiffFile>,
    branch_diff: Vec<DiffFile>,
    branch_patch: String,
    branch_info: git::BranchInfo,
    right: RightPane,
    /// The file the Structure panel shows (tab id), and its outline cached by
    /// (tab id, edit version).
    structure_file: Option<usize>,
    /// Keyboard focus of the side panel (Alt+1/3), and the row the arrows are on.
    side_focus: FocusHandle,
    side_selected: usize,
    /// Whether the side panel holds the keyboard, set each render.
    side_has_focus: std::cell::Cell<bool>,
    structure: std::cell::RefCell<Option<(usize, u64, Rc<Vec<crate::finder::OutlineItem>>)>>,
    tree: Entity<TreeState>,
    /// The file list the tree was built from, and the folders open in it.
    tree_files: Vec<String>,
    /// The same list for definition lookups running off the UI thread.
    shared_files: Arc<std::sync::RwLock<Vec<String>>>,
    expanded: HashSet<SharedString>,
    file_states: HashMap<String, git::FileState>,
    commit_input: Entity<gpui_kit::component::input::TextareaState>,
    /// The git/gh action running, and the outcome of the last one.
    busy: Option<&'static str>,
    action: Option<(bool, String)>,
    /// Wakes the refresh loop: file events, actions, the fallback tick.
    refresh_tx: async_channel::Sender<()>,
    /// The branch "Branch vs …" compares with, when picked by hand; shared
    /// with the refresh loop.
    base_override: Arc<std::sync::Mutex<Option<String>>>,
    watcher: Option<notify::RecommendedWatcher>,
    watched: (BTreeSet<PathBuf>, Vec<PathBuf>),
    /// One server per (program, project root).
    lsp: HashMap<(&'static str, PathBuf), Arc<lsp::LspClient>>,
    visible: bool,
    _poll: Task<()>,
}

impl TaskView {
    pub fn new(
        root: PathBuf,
        branch: String,
        agent: Option<String>,
        hook_socket: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.weak_entity();
        let changes = cx.new(|cx| ChangesView::new(root.clone(), weak.clone(), cx));
        let history = cx.new(|cx| HistoryView::new(root.clone(), weak, cx));
        let tree = cx.new(|cx| TreeState::new(cx));
        cx.subscribe(&tree, |this: &mut Self, _, event: &TreeEvent, _| match event {
            TreeEvent::Expanded(id) => {
                this.expanded.insert(id.clone());
            }
            TreeEvent::Collapsed(id) => {
                this.expanded.remove(id);
            }
        })
        .detach();

        let mut tabs = Vec::new();
        // The agent's terminal, or a shell when there is no agent; more shells
        // are a "+" away.
        match agent {
            Some(agent) => tabs.push(Self::make_terminal(&root, hook_socket.as_ref(), "agent", 0, Some(agent), window, cx)),
            None => tabs.push(Self::make_terminal(&root, hook_socket.as_ref(), "shell", 1, None, window, cx)),
        }
        // Changes and History open on demand: the Log button, a changed file,
        // Ctrl+2/3.

        let (refresh_tx, refresh_rx) = async_channel::unbounded::<()>();
        let watch_tx = refresh_tx.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event {
                if !matches!(event.kind, notify::EventKind::Access(_)) {
                    let _ = watch_tx.try_send(());
                }
            }
        })
        .inspect_err(|e| eprintln!("file watcher off, polling only: {e}"))
        .ok();
        // Fallback for anything the watcher misses (new directories, remotes).
        let tick_tx = refresh_tx.clone();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(10)).await;
                if tick_tx.send(()).await.is_err() {
                    break;
                }
            }
        })
        .detach();

        let poll_root = root.clone();
        let base_override: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let poll_base = base_override.clone();
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                let root = poll_root.clone();
                let picked = poll_base.lock().unwrap().clone();
                let (patch, files, info, states, watch, branch) = cx
                    .background_spawn(async move {
                        let files = git::files(&root);
                        let watch = git::watch_dirs(&root, &files);
                        let branch = picked
                            .or_else(|| git::base_ref(&root))
                            .and_then(|base| git::branch_patch(&root, &base).map(|p| (base, p)));
                        (git::working_patch(&root), files, git::branch_info(&root), git::file_states(&root), watch, branch)
                    })
                    .await;
                let applied = this.update_in(cx, |this, window, cx| {
                    this.check_open_files(window, cx);
                    this.file_states = states;
                    this.apply_patch(patch, cx);
                    this.apply_branch_patch(branch, cx);
                    this.apply_files(files, cx);
                    this.apply_branch(info, cx);
                    this.sync_watches(watch);
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
                // Wait for a change, then let a burst of events settle.
                if refresh_rx.recv().await.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(150)).await;
                while refresh_rx.try_recv().is_ok() {}
            }
        });

        // Grows with the message, up to eight lines.
        let commit_input = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx).auto_grow(1, 8).placeholder("Commit message")
        });

        let mut this = Self {
            root,
            branch: branch.into(),
            agent_status: crate::agent::AgentStatus::Unknown,
            hook_socket,
            terminal_seq: 1,
            unseen: false,
            tabs,
            left_active: 0,
            right_active: None,
            focus_right: false,
            zoomed: None,
            last_tab_click: None,
            focus: cx.focus_handle(),
            changes,
            history,
            files: Vec::new(),
            patch: String::new(),
            uncommitted_diff: Vec::new(),
            branch_diff: Vec::new(),
            branch_patch: String::new(),
            branch_info: git::BranchInfo::default(),
            right: RightPane::Changes,
            structure_file: None,
            side_focus: cx.focus_handle(),
            side_selected: 0,
            side_has_focus: Default::default(),
            structure: Default::default(),
            tree,
            tree_files: Vec::new(),
            shared_files: Arc::default(),
            expanded: HashSet::new(),
            file_states: HashMap::new(),
            commit_input,
            busy: None,
            action: None,
            refresh_tx,
            base_override,
            watcher,
            watched: Default::default(),
            lsp: HashMap::new(),
            visible: false,
            _poll: poll,
        };
        // Debug aid for headless screenshots: start on a given tab.
        let first = std::env::var("IDEAE_TAB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        this.left_active = this.tabs.get(first).or(this.tabs.first()).map_or(0, |t| t.id);
        this.sync_visibility(cx);
        this
    }

    fn make_terminal(
        root: &PathBuf,
        hook_socket: Option<&PathBuf>,
        title: &str,
        seq: usize,
        input: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabItem {
        let mut options = LocalTerminalOptions::default().with_cwd(WorkingDirectory::Path(root.clone()));
        // Every terminal of the task can reach the app's hook socket, so a
        // claude started by hand reports too once given the settings file.
        if let Some(socket) = hook_socket {
            options = options
                .with_env(crate::agent::SOCK_ENV, socket.display().to_string())
                .with_env(crate::agent::TASK_ENV, root.display().to_string());
        }
        if let Some(input) = input {
            options = options.with_input(format!("{input}\n"));
        }
        let config = TerminalConfig::default().with_colors(crate::theme::terminal_colors(cx));
        let state = cx.new(|cx| TerminalState::local(options, config, window, cx));
        cx.subscribe_in(&state, window, |this: &mut Self, state, event: &TerminalEvent, window, cx| match event {
            // A shell that exits (Ctrl+D, `exit`) takes its tab with it, as in a
            // terminal app; the agent's tab stays and shows the exit.
            TerminalEvent::Exited(_) => {
                let ix = this.tabs.iter().position(|t| match &t.kind {
                    TabKind::Terminal(v) => v.read(cx).state == *state,
                    _ => false,
                });
                if let Some(ix) = ix.filter(|&ix| this.tabs[ix].title != "agent") {
                    this.remove_tab(ix, window, cx);
                }
                cx.emit(TaskEvent::Changed);
                cx.notify();
            }
            TerminalEvent::TitleChanged | TerminalEvent::Bell => {
                cx.emit(TaskEvent::Changed);
                cx.notify();
            }
            TerminalEvent::OpenPath(target) => {
                let cwd = state.read(cx).cwd().map(|p| p.to_path_buf());
                this.open_terminal_path(target, cwd, window, cx);
            }
            _ => {}
        })
        .detach();
        let id = SharedString::from(format!("{}-term-{seq}", root.display()));
        let view = cx.new(|cx| TermView::new(id, state, cx));
        TabItem::new(title.to_string(), TabKind::Terminal(view))
    }

    fn ix_of(&self, id: usize) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn is_split(&self) -> bool {
        self.tabs.iter().any(|t| t.right)
    }

    /// Keeps the panes consistent after tabs move or close: an empty left pane
    /// takes the right one's tabs (the split closes), and each pane shows one
    /// of its own tabs.
    fn normalize(&mut self) {
        if self.tabs.iter().all(|t| t.right) {
            for tab in &mut self.tabs {
                tab.right = false;
            }
        }
        let shows = |id: Option<usize>, right: bool, tabs: &[TabItem]| {
            id.is_some_and(|id| tabs.iter().any(|t| t.id == id && t.right == right))
        };
        if !shows(Some(self.left_active), false, &self.tabs) {
            self.left_active = self.tabs.iter().find(|t| !t.right).map_or(0, |t| t.id);
        }
        if !shows(self.right_active, true, &self.tabs) {
            self.right_active = self.tabs.iter().find(|t| t.right).map(|t| t.id);
        }
        if self.right_active.is_none() {
            self.focus_right = false;
        }
    }

    /// Moves a tab to the other pane and shows it there, splitting if needed.
    fn move_to_other_pane(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(ix) else { return };
        // The only tab on the left cannot leave it: there would be nothing to split from.
        if !tab.right && self.tabs.iter().filter(|t| !t.right).count() < 2 {
            return;
        }
        let tab = &mut self.tabs[ix];
        tab.right = !tab.right;
        let (id, right) = (tab.id, tab.right);
        if right {
            self.right_active = Some(id);
        } else {
            self.left_active = id;
        }
        self.focus_right = right;
        self.normalize();
        self.sync_visibility(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// The right pane's tabs all go back to the left one.
    fn join_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for tab in &mut self.tabs {
            tab.right = false;
        }
        self.normalize();
        self.sync_visibility(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// The zoomed pane, for the window to float over everything. While it is
    /// up, the task's own layout leaves that pane's slot empty: a view (a
    /// terminal above all) is drawn in one place only.
    pub fn render_zoomed(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let right = self.zoomed?;
        Some(
            div()
                .size_full()
                .track_focus(&self.focus)
                .on_action(cx.listener(|this, _: &ToggleZoom, window, cx| this.toggle_zoom_focused(window, cx)))
                .on_action(cx.listener(|this, _: &ShowOutline, window, cx| this.show_outline(window, cx)))
                .on_action(cx.listener(|this, _: &SendToAgent, window, cx| this.send_editor_selection(window, cx)))
                .child(self.render_pane(right, cx))
                .into_any_element(),
        )
    }

    /// Focus mode for a pane, like Zed's: it takes the whole window, over the
    /// sidebars and the other pane. The same gesture brings everything back.
    /// Shift+Esc: focus mode for the pane you are in.
    pub fn toggle_zoom_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let right = self.focus_right;
        self.toggle_zoom(right, window, cx)
    }

    fn toggle_zoom(&mut self, right: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.zoomed = if self.zoomed.is_some() { None } else { Some(right && self.is_split()) };
        self.focus_right = right && self.is_split();
        self.focus_active(window, cx);
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    fn focus_pane(&mut self, right: bool, cx: &mut Context<Self>) {
        if self.focus_right != right && (!right || self.is_split()) {
            self.focus_right = right;
            self.autosave_unfocused(cx);
            cx.notify();
        }
    }

    fn tab_content(&self, ix: Option<usize>) -> AnyElement {
        match ix.and_then(|ix| self.tabs.get(ix)).map(|t| &t.kind) {
            Some(TabKind::Terminal(view)) => view.clone().into_any_element(),
            Some(TabKind::Changes) => self.changes.clone().into_any_element(),
            Some(TabKind::History) => self.history.clone().into_any_element(),
            Some(TabKind::File(file)) => file.clone().into_any_element(),
            Some(TabKind::Browser(view)) => view.clone().into_any_element(),
            Some(TabKind::Binary(view)) => view.clone().into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// A tab dropped on a pane moves there (or just shows, if already there).
    fn drop_tab(&mut self, tab_id: usize, right: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.ix_of(tab_id) else { return };
        if self.tabs[ix].right == right {
            self.activate(ix, window, cx);
        } else {
            self.move_to_other_pane(ix, window, cx);
        }
    }

    /// One pane: its own tab bar over the tab it shows.
    fn render_pane(&self, right: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.is_split() && self.focus_right == right;
        let shown = if right { self.right_active } else { Some(self.left_active) };
        let highlight = cx.theme().primary.opacity(0.18);
        // While a tab is dragged, a transparent layer over the pane takes the
        // drop and shows the highlight: content such as a terminal paints an
        // opaque background over anything set on the pane itself. Unsplit,
        // the right edge is a separate zone that opens the right pane.
        let dragging = tab_drag_active(cx);
        let split_zone = !self.is_split() && dragging;
        v_flex()
            .id(if right { "pane-right" } else { "pane-left" })
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .capture_any_mouse_down(cx.listener(move |this, _, _, cx| this.focus_pane(right, cx)))
            .child(
                div()
                    .border_t_2()
                    .border_color(if focused { cx.theme().primary } else { cx.theme().transparent })
                    .child(self.render_tabs(right, cx)),
            )
            .child(div().flex_1().min_h_0().child(self.tab_content(shown.and_then(|id| self.ix_of(id)))))
            .when(dragging, |pane| {
                pane.child(
                    div()
                        .id("drop-layer")
                        .absolute()
                        .top_0()
                        .left_0()
                        .h_full()
                        .w(if split_zone { relative(0.7) } else { relative(1.) })
                        .drag_over::<DraggedTab>(move |style, _, _, _| style.bg(highlight))
                        .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                            this.drop_tab(dragged.tab_id, right, window, cx)
                        })),
                )
            })
            .when(split_zone, |pane| {
                pane.child(
                    div()
                        .id("split-zone")
                        .absolute()
                        .top_0()
                        .right_0()
                        .h_full()
                        .w(relative(0.3))
                        .border_l_2()
                        .border_color(cx.theme().primary)
                        .drag_over::<DraggedTab>(move |style, _, _, _| style.bg(highlight))
                        .on_drop(cx.listener(|this, dragged: &DraggedTab, window, cx| {
                            if let Some(ix) = this.ix_of(dragged.tab_id) {
                                this.move_to_other_pane(ix, window, cx);
                            }
                        }))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Drop to split"),
                )
            })
    }

    /// Puts code into the agent's prompt, without sending it: the location,
    /// then the code fenced, so you can type the question after it.
    pub fn send_to_agent(&mut self, location: String, code: String, language: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tabs.iter().position(|t| t.title == "agent" && matches!(t.kind, TabKind::Terminal(_))) else {
            return;
        };
        let TabKind::Terminal(view) = &self.tabs[ix].kind else { return };
        let state = view.read(cx).state.clone();
        let text = format!("{location}\n```{language}\n{}\n```\n", code.trim_end_matches('\n'));
        // Bracketed paste: Claude Code shows it as pasted text, not a prompt sent.
        state.update(cx, |s, cx| s.paste_text(text, cx));
        self.activate(ix, window, cx);
    }

    fn send_editor_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(TabKind::File(file)) = self.focused_ix().map(|ix| &self.tabs[ix].kind) else { return };
        let (path, text, range) = {
            let f = file.read(cx);
            let state = f.editor.read(cx);
            (f.path.clone(), state.text().to_string(), state.selected_range())
        };
        // Nothing selected: the line the cursor is on.
        let (start, end) = if range.is_empty() {
            let s = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
            let e = text[range.start..].find('\n').map_or(text.len(), |i| range.start + i);
            (s, e)
        } else {
            (range.start, range.end)
        };
        let line_of = |offset: usize| text[..offset].matches('\n').count() + 1;
        let (first, last) = (line_of(start), line_of(end.max(start + 1).min(text.len()).saturating_sub(if end > start { 1 } else { 0 })));
        let rel = path.strip_prefix(&self.root).unwrap_or(&path).display().to_string();
        let location = if first == last { format!("{rel}:{first}") } else { format!("{rel}:{first}-{last}") };
        let language = lsp::language_for(&path);
        self.send_to_agent(location, text[start..end].to_string(), language, window, cx);
    }

    /// Ctrl+F12: the declarations of the file in view, to jump to one.
    fn show_outline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(TabKind::File(file)) = self.focused_ix().map(|ix| &self.tabs[ix].kind) else { return };
        let (path, text) = {
            let f = file.read(cx);
            (f.path.clone(), f.editor.read(cx).text().to_string())
        };
        let items = outline(&path, &text);
        let shown = path.strip_prefix(&self.root).unwrap_or(&path).display().to_string();
        crate::finder::Finder::open_outline(self.root.clone(), shown, items, cx.weak_entity(), window, cx);
    }

    /// Opens History on one commit (from a blame line).
    pub fn show_commit(&mut self, hash: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.show_tab(|k| matches!(k, TabKind::History), window, cx);
        self.history.update(cx, |h, cx| h.select_hash(hash, cx));
    }

    /// Files dropped on the window from a file manager open as tabs.
    pub fn open_dropped(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        for path in paths.iter().filter(|p| p.is_file()) {
            self.open_file(path.clone(), 0, window, cx);
        }
    }

    pub fn open_finder(&mut self, kind: crate::finder::FinderKind, window: &mut Window, cx: &mut Context<Self>) {
        let files = Arc::new(self.tree_files.clone());
        crate::finder::Finder::open(kind, self.root.clone(), files, cx.weak_entity(), window, cx);
    }

    /// A browser tab in the pane you are in. Playwright MCP can drive the
    /// same page through the endpoint shown in its toolbar.
    fn new_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_browser("about:blank", window, cx);
    }

    pub fn open_browser(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| crate::browser::BrowserView::new(url, window, cx));
        cx.subscribe(&view, |_, _, _: &crate::browser::BrowserEvent, cx| cx.notify()).detach();
        let mut tab = TabItem::new("web", TabKind::Browser(view));
        tab.right = self.focus_right && self.is_split();
        self.tabs.push(tab);
        self.activate(self.tabs.len() - 1, window, cx);
    }

    /// A Ctrl+clicked `path[:line]` from terminal output: relative to the
    /// terminal's directory, else the worktree.
    fn open_terminal_path(&mut self, target: &str, cwd: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(symbol) = target.strip_prefix("symbol:") {
            return self.open_symbol(symbol, window, cx);
        }
        let (path, line) = match target.rsplit_once(':') {
            Some((path, line)) if line.parse::<u32>().is_ok() => (path, line.parse::<u32>().unwrap_or(1)),
            _ => (target, 1),
        };
        let path = path.strip_prefix("~/").map_or_else(
            || PathBuf::from(path),
            |rest| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        );
        let candidates = if path.is_absolute() {
            vec![path.clone()]
        } else {
            cwd.into_iter().map(|d| d.join(&path)).chain([self.root.join(&path)]).collect()
        };
        let found = candidates.into_iter().find(|p| p.is_file()).or_else(|| {
            // Agents and compilers often print a bare name ("Providers.kt:554")
            // or a partial path: match the end of a project file's path.
            let needle = path.to_string_lossy().trim_start_matches("./").to_string();
            let suffix = format!("/{needle}");
            self.tree_files
                .iter()
                .filter(|f| **f == needle || f.ends_with(&suffix))
                .min_by_key(|f| f.len())
                .map(|f| self.root.join(f))
        });
        if let Some(found) = found {
            self.open_file(found, line.saturating_sub(1), window, cx);
        }
    }

    /// `Class.member` or `Class` from terminal output: a file of that name, or
    /// one named after the class (then the member by its outline), or the
    /// class's declaration found by ripgrep.
    fn open_symbol(&mut self, symbol: &str, window: &mut Window, cx: &mut Context<Self>) {
        let places = find_symbol(&self.root, &self.tree_files, symbol);
        match places.len() {
            0 => {}
            1 => {
                let (path, line, _) = places.into_iter().next().unwrap();
                self.open_file(path, line, window, cx);
            }
            _ => {
                let hits = places
                    .into_iter()
                    .map(|(path, line, code)| {
                        let shown = path.strip_prefix(&self.root).unwrap_or(&path).display().to_string();
                        crate::finder::Hit { text: format!("{}   {shown}", code.trim()), path: shown, line: line + 1 }
                    })
                    .collect();
                crate::finder::Finder::open_hits(self.root.clone(), hits, cx.weak_entity(), window, cx)
            }
        }
    }

    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.new_terminal_tab(window, cx);
        self.tabs[at].right = self.focus_right && self.is_split();
        self.activate(at, window, cx);
    }

    /// Adds a shell tab after the last terminal and returns its index.
    fn new_terminal_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        self.terminal_seq += 1;
        let seq = self.terminal_seq;
        let tab = Self::make_terminal(&self.root, self.hook_socket.as_ref(), &format!("shell {seq}"), seq, None, window, cx);
        // After the last terminal, so terminals stay together left of the views.
        let at = self.tabs.iter().rposition(|t| matches!(t.kind, TabKind::Terminal(_))).map_or(0, |ix| ix + 1);
        self.tabs.insert(at, tab);
        at
    }

    /// The agent terminal's title, which Claude Code uses as a status line.
    pub fn agent_title(&self, cx: &App) -> Option<SharedString> {
        self.tabs.iter().find_map(|tab| match &tab.kind {
            TabKind::Terminal(view) if tab.title == "agent" => {
                Some(view.read(cx).state.read(cx).title().clone())
            }
            _ => None,
        })
    }

    /// You left a file (another tab, the other pane): save it, JetBrains-style.
    /// The editor sends no blur events here, so the task tracks it instead.
    fn autosave_unfocused(&mut self, cx: &mut Context<Self>) {
        let focused = self.focused_ix().map(|ix| self.tabs[ix].id);
        for tab in &self.tabs {
            if let TabKind::File(file) = &tab.kind {
                if Some(tab.id) != focused {
                    file.update(cx, |f, cx| f.autosave(cx));
                }
            }
        }
    }

    pub fn autosave_all(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let TabKind::File(file) = &tab.kind {
                file.update(cx, |f, cx| f.autosave(cx));
            }
        }
    }

    pub fn set_terminal_colors(&mut self, colors: &gpui_cn::terminal::TerminalColors, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let TabKind::Terminal(view) = &tab.kind {
                let state = view.read(cx).state.clone();
                state.update(cx, |s, cx| s.set_colors(colors.clone(), cx));
            }
        }
    }

    pub fn set_agent_status(&mut self, status: crate::agent::AgentStatus, seen: bool, cx: &mut Context<Self>) {
        self.unseen = status.is_attention() && !seen;
        self.agent_status = status;
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    pub fn change_count(&self) -> usize {
        self.files.len()
    }

    pub fn set_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = visible;
        self.sync_visibility(cx);
        if visible {
            self.focus_active(window, cx);
        }
    }

    /// Hidden terminals keep parsing but stop producing frames.
    fn sync_visibility(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let TabKind::Terminal(view) = &tab.kind {
                let shown = tab.id == self.left_active || Some(tab.id) == self.right_active;
                let visible = self.visible && shown;
                view.read(cx).state.clone().update(cx, |s, cx| s.set_visible(visible, cx));
            }
        }
    }

    /// The tab in the pane that has focus.
    fn focused_ix(&self) -> Option<usize> {
        let id = if self.focus_right { self.right_active } else { Some(self.left_active) };
        id.and_then(|id| self.ix_of(id))
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.focused_ix().and_then(|ix| self.tabs.get(ix)).map(|t| &t.kind) {
            Some(TabKind::Terminal(view)) => {
                view.read(cx).state.read(cx).focus_handle().clone().focus(window, cx)
            }
            Some(TabKind::File(file)) => {
                file.read(cx).editor.read(cx).focus_handle(cx).focus(window, cx)
            }
            Some(TabKind::Browser(view)) => view.focus_handle(cx).focus(window, cx),
            _ => self.focus.focus(window, cx),
        }
    }

    /// Shows tab `ix` in its own pane and focuses that pane.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else { return };
        if matches!(tab.kind, TabKind::File(_)) {
            self.structure_file = Some(tab.id);
        }
        if tab.right {
            self.right_active = Some(tab.id);
            self.focus_right = true;
        } else {
            self.left_active = tab.id;
            self.focus_right = false;
        }
        self.autosave_unfocused(cx);
        self.sync_visibility(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.get(ix).map(|t| &t.kind) {
            Some(TabKind::File(file)) if file.read(cx).dirty => {
                let title = self.tabs[ix].title.clone();
                let task = cx.weak_entity();
                let file_id = file.entity_id();
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let task = task.clone();
                    dialog
                        .confirm()
                        .title(format!("Discard changes to {title}?"))
                        .description("The unsaved edits will be lost.")
                        .ok_text("Discard")
                        .on_ok(move |_, window, cx| {
                            let _ = task.update(cx, |t, cx| {
                                if let Some(ix) = t.tabs.iter().position(|tab| match &tab.kind {
                                    TabKind::File(f) => f.entity_id() == file_id,
                                    _ => false,
                                }) {
                                    t.remove_tab(ix, window, cx);
                                }
                            });
                            true
                        })
                })
            }
            Some(TabKind::File(_) | TabKind::Browser(_) | TabKind::Binary(_) | TabKind::Changes | TabKind::History) => {
                self.remove_tab(ix, window, cx)
            }
            Some(TabKind::Terminal(_)) if self.tabs[ix].title == "agent" => {}
            Some(TabKind::Terminal(view)) => {
                let state = view.read(cx).state.clone();
                if !state.read(cx).needs_confirm_close() {
                    state.update(cx, |s, cx| s.close(cx));
                    self.remove_tab(ix, window, cx);
                    return;
                }
                // Something other than the shell runs there (claude, a server…).
                let program = state
                    .read(cx)
                    .foreground()
                    .map(|p| format!("`{}`", p.name()))
                    .unwrap_or_else(|| "a program".into());
                let title = self.tabs[ix].title.clone();
                let task = cx.weak_entity();
                let view_id = view.entity_id();
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let task = task.clone();
                    let state = state.clone();
                    dialog
                        .confirm()
                        .title(format!("Close \"{title}\"?"))
                        .description(format!("It is still running {program}, which will be stopped."))
                        .ok_text("Close")
                        .on_ok(move |_, window, cx| {
                            state.update(cx, |s, cx| s.close(cx));
                            let _ = task.update(cx, |t, cx| {
                                if let Some(ix) = t.tabs.iter().position(|tab| match &tab.kind {
                                    TabKind::Terminal(v) => v.entity_id() == view_id,
                                    _ => false,
                                }) {
                                    t.remove_tab(ix, window, cx);
                                }
                            });
                            true
                        })
                })
            }
            _ => {}
        }
    }

    fn remove_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let removed = self.tabs.remove(ix);
        // The pane that showed it moves to its left neighbour in that pane, not
        // onto whatever comes next (a diff the same click would hit).
        let neighbour = self.tabs[..ix.min(self.tabs.len())]
            .iter()
            .rev()
            .chain(self.tabs[ix.min(self.tabs.len())..].iter())
            .find(|t| t.right == removed.right)
            .map(|t| t.id);
        if removed.right {
            if self.right_active == Some(removed.id) {
                self.right_active = neighbour;
            }
        } else if self.left_active == removed.id {
            self.left_active = neighbour.unwrap_or(0);
        }
        self.normalize();
        self.sync_visibility(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn apply_patch(&mut self, patch: String, cx: &mut Context<Self>) {
        if patch == self.patch {
            return;
        }
        let files = match DiffFile::parse(&patch) {
            Ok(files) => files,
            Err(error) => {
                eprintln!("diff parse: {error}");
                return;
            }
        };
        let first_lines = git::first_changed_lines(&patch);
        self.patch = patch;
        self.files = files
            .iter()
            .map(|f| ChangedFile {
                path: f.path().clone(),
                first_line: first_lines.get(f.path().as_ref()).copied().unwrap_or(1),
                status: f.status(),
                additions: f.additions(),
                deletions: f.deletions(),
            })
            .collect();
        self.uncommitted_diff = with_languages(files);
        self.refresh_scope(DiffScope::Uncommitted, cx);
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    fn apply_branch_patch(&mut self, branch: Option<(String, String)>, cx: &mut Context<Self>) {
        let (base, patch) = match branch {
            Some((base, patch)) => (Some(base), patch),
            None => (None, String::new()),
        };
        let base_changed = self.changes.read(cx).base != base;
        if patch == self.branch_patch && !base_changed {
            return;
        }
        self.branch_diff = with_languages(DiffFile::parse(&patch).unwrap_or_default());
        self.branch_patch = patch;
        self.changes.update(cx, |c, _| c.base = base);
        self.refresh_scope(DiffScope::Branch, cx);
    }

    /// New data for `changed`: reload the diff only if that scope is on screen,
    /// so scrolling the other one is not reset.
    fn refresh_scope(&mut self, changed: DiffScope, cx: &mut Context<Self>) {
        if self.changes.read(cx).scope == changed {
            self.show_scope(cx);
        } else {
            let counts = (self.uncommitted_diff.len(), self.branch_diff.len());
            self.changes.update(cx, |c, cx| {
                c.counts = counts;
                cx.notify();
            });
        }
    }

    pub fn set_base(&mut self, base: String) {
        *self.base_override.lock().unwrap() = Some(base);
        let _ = self.refresh_tx.try_send(());
    }

    /// Puts the selected scope's files into the Changes diff.
    fn show_scope(&mut self, cx: &mut Context<Self>) {
        let counts = (self.uncommitted_diff.len(), self.branch_diff.len());
        let scope = self.changes.read(cx).scope;
        let (files, patch) = match scope {
            DiffScope::Uncommitted => (self.uncommitted_diff.clone(), &self.patch),
            DiffScope::Branch => (self.branch_diff.clone(), &self.branch_patch),
        };
        let lines = Rc::new(git::first_changed_lines(patch));
        let diff = self.changes.read(cx).diff.clone();
        diff.update(cx, |state, cx| state.set_files(files, cx));
        self.changes.update(cx, |c, cx| {
            c.counts = counts;
            c.first_lines = lines;
            cx.notify();
        });
    }

    fn check_open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let files: Vec<Entity<FileView>> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::File(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        for file in files {
            file.update(cx, |f, cx| f.check_disk(window, cx));
        }
    }

    fn sync_watches(&mut self, watch: (BTreeSet<PathBuf>, Vec<PathBuf>)) {
        if watch == self.watched {
            return;
        }
        let Some(watcher) = self.watcher.as_mut() else { return };
        let (old_flat, old_rec) = std::mem::take(&mut self.watched);
        for dir in old_flat.difference(&watch.0) {
            let _ = watcher.unwatch(dir);
        }
        for dir in watch.0.difference(&old_flat) {
            let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
        }
        for dir in old_rec.iter().filter(|d| !watch.1.contains(d)) {
            let _ = watcher.unwatch(dir);
        }
        for dir in watch.1.iter().filter(|d| !old_rec.contains(d)) {
            let _ = watcher.watch(dir, RecursiveMode::Recursive);
        }
        self.watched = watch;
    }

    fn apply_files(&mut self, files: Vec<String>, cx: &mut Context<Self>) {
        if files == self.tree_files {
            return;
        }
        let items = build_tree(&files, &self.expanded);
        *self.shared_files.write().unwrap() = files.clone();
        // Class names for terminal links: `Profit` in output links to Profit.kt.
        gpui_cn::terminal::add_known_names(files.iter().filter_map(|f| {
            let name = f.rsplit('/').next()?;
            let stem = name.split('.').next()?;
            (stem.len() > 2 && stem.chars().next()?.is_ascii_uppercase()).then(|| stem.to_string())
        }));
        self.tree_files = files;
        self.tree.update(cx, |t, cx| t.set_items(items, cx));
    }

    fn apply_branch(&mut self, info: git::BranchInfo, cx: &mut Context<Self>) {
        if info == self.branch_info {
            return;
        }
        if !info.head.is_empty() {
            self.branch = info.head.clone().into();
        }
        self.history.update(cx, |h, cx| h.sync_head(&info.last_hash, cx));
        // A new commit changes who last touched the lines.
        if info.last_hash != self.branch_info.last_hash {
            for tab in &self.tabs {
                if let TabKind::File(file) = &tab.kind {
                    file.update(cx, |f, cx| f.load_blame(cx));
                }
            }
        }
        self.branch_info = info;
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    pub fn show_view(&mut self, which: usize, window: &mut Window, cx: &mut Context<Self>) {
        match which {
            0 => {
                if let Some(ix) = self.tabs.iter().position(|t| t.title == "agent") {
                    self.activate(ix, window, cx);
                }
            }
            1 => self.show_tab(|k| matches!(k, TabKind::Changes), window, cx),
            2 => self.show_tab(|k| matches!(k, TabKind::History), window, cx),
            _ => match self.tabs.iter().position(|t| t.title != "agent" && matches!(t.kind, TabKind::Terminal(_))) {
                Some(ix) => self.activate(ix, window, cx),
                None => self.new_terminal(window, cx),
            },
        }
    }

    /// Ctrl+PgUp/PgDn: the neighbouring tab in the focused pane, wrapping.
    pub fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let right = self.focus_right && self.is_split();
        let in_pane: Vec<usize> = (0..self.tabs.len()).filter(|&ix| self.tabs[ix].right == right).collect();
        let Some(current) = self.focused_ix().and_then(|ix| in_pane.iter().position(|&i| i == ix)) else { return };
        let next = if forward { (current + 1) % in_pane.len() } else { (current + in_pane.len() - 1) % in_pane.len() };
        self.activate(in_pane[next], window, cx);
    }

    /// Alt+1/2/3: shows the panel and moves the keyboard there. Arrows move,
    /// Enter opens, Escape goes back to the tab.
    pub fn show_side_panel(&mut self, which: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.right = match which {
            0 => RightPane::Changes,
            1 => RightPane::Files,
            _ => RightPane::Structure,
        };
        self.side_selected = match self.right {
            // Start on the declaration the cursor is in.
            RightPane::Structure => self.structure_items(cx).iter().rposition(|i| i.line <= self.structure_cursor(cx)).unwrap_or(0),
            _ => 0,
        };
        match self.right {
            RightPane::Files => self.tree.update(cx, |t, cx| t.focus(window, cx)),
            _ => self.side_focus.focus(window, cx),
        }
        cx.notify();
    }

    fn side_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let len = match self.right {
            RightPane::Changes => self.files.len(),
            RightPane::Structure => self.structure_items(cx).len(),
            RightPane::Files => return,
        };
        match event.keystroke.key.as_str() {
            "down" if len > 0 => self.side_selected = (self.side_selected + 1).min(len - 1),
            "up" => self.side_selected = self.side_selected.saturating_sub(1),
            "enter" if len > 0 => {
                let ix = self.side_selected.min(len - 1);
                match self.right {
                    RightPane::Changes => {
                        let path = self.files[ix].path.clone();
                        self.show_changed_file(path, window, cx);
                    }
                    _ => {
                        let line = self.structure_items(cx)[ix].line;
                        if let Some((id, file)) = self.structure_target() {
                            if let Some(tab) = self.tabs.iter().position(|t| t.id == id) {
                                self.activate(tab, window, cx);
                            }
                            file.update(cx, |f, cx| f.go_to_line(line, window, cx));
                        }
                    }
                }
                return;
            }
            "escape" => return self.focus_active(window, cx),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn structure_target(&self) -> Option<(usize, Entity<FileView>)> {
        let id = self.structure_file?;
        self.tabs.iter().find_map(|t| match &t.kind {
            TabKind::File(f) if t.id == id => Some((id, f.clone())),
            _ => None,
        })
    }

    fn structure_cursor(&self, cx: &App) -> u32 {
        self.structure_target().map_or(0, |(_, f)| f.read(cx).cursor_line)
    }

    /// The outline of the Structure file, parsed again only after edits.
    fn structure_items(&self, cx: &App) -> Rc<Vec<crate::finder::OutlineItem>> {
        let Some((id, file)) = self.structure_target() else { return Rc::default() };
        let (path, version) = {
            let f = file.read(cx);
            (f.path.clone(), f.edit_version)
        };
        if let Some((_, _, items)) = self.structure.borrow().as_ref().filter(|(i, v, _)| *i == id && *v == version) {
            return items.clone();
        }
        let text = file.read(cx).editor.read(cx).text().to_string();
        let items = Rc::new(outline(&path, &text));
        *self.structure.borrow_mut() = Some((id, version, items.clone()));
        items
    }

    /// Shows the Changes or History tab, opening it in the pane you are in
    /// when it is not open yet.
    fn show_tab(&mut self, pick: impl Fn(&TabKind) -> bool, window: &mut Window, cx: &mut Context<Self>) {
        let ix = match self.tabs.iter().position(|t| pick(&t.kind)) {
            Some(ix) => ix,
            None => {
                let (title, kind) = if pick(&TabKind::Changes) {
                    ("Changes", TabKind::Changes)
                } else if pick(&TabKind::History) {
                    ("History", TabKind::History)
                } else {
                    return;
                };
                let mut tab = TabItem::new(title, kind);
                tab.right = self.focus_right && self.is_split();
                self.tabs.push(tab);
                self.tabs.len() - 1
            }
        };
        self.activate(ix, window, cx);
    }

    fn render_branch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let b = &self.branch_info;
        h_flex()
            .id("branch")
            .gap_2()
            .px_3()
            .py_1()
            .text_xs()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().font_semibold().child(format!("⎇ {}", if b.head.is_empty() { "…" } else { &b.head })))
            .when_some(b.upstream.clone(), |this, up| {
                this.child(div().text_color(cx.theme().muted_foreground).child(format!("→ {up}")))
                    .when(b.ahead > 0, |this| this.child(div().text_color(gpui_kit::green()).child(format!("↑{}", b.ahead))))
                    .when(b.behind > 0, |this| this.child(div().text_color(gpui_kit::red()).child(format!("↓{}", b.behind))))
            })
            .when(b.upstream.is_none() && !b.head.is_empty(), |this| {
                this.child(div().text_color(cx.theme().muted_foreground).child("no upstream"))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(cx.theme().muted_foreground)
                    .cursor_pointer()
                    .child(format!("· {} {}", b.last_hash, b.last_subject)),
            )
            .child(
                Button::new("log")
                    .xsmall()
                    .ghost()
                    .label("Log")
                    .tooltip("The branch's history (Ctrl+3)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.show_tab(|k| matches!(k, TabKind::History), window, cx)
                    })),
            )
            .when(b.upstream.is_some(), |bar| {
                let busy = self.busy.is_some();
                bar.child(
                    Button::new("fetch")
                        .xsmall()
                        .ghost()
                        .disabled(busy)
                        .label("Fetch")
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.run_action("Fetch", |root| git::fetch(&root), false, window, cx)
                        })),
                )
                .child(
                    Button::new("pull")
                        .xsmall()
                        .ghost()
                        .disabled(busy)
                        .label(if b.behind > 0 { format!("Pull ↓{}", b.behind) } else { "Pull".to_string() })
                        .tooltip("Fast-forward, or rebase local commits; undone on any conflict")
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.run_action("Pull", |root| git::pull_safe(&root), false, window, cx)
                        })),
                )
            })
            // The branch's last commit, in History.
            .on_click(cx.listener(|this, _, window, cx| {
                let hash = this.branch_info.last_hash.clone();
                this.show_commit(&hash, window, cx)
            }))
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        let root = self.root.clone();
        let changed: Rc<HashSet<SharedString>> = Rc::new(self.files.iter().map(|f| f.path.clone()).collect());
        div().flex_1().min_h_0().child(
            tree(&self.tree, move |ix, entry, selected, _, _| {
                let item = entry.item();
                let icon = if !entry.is_folder() {
                    IconName::File
                } else if entry.is_expanded() {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                };
                let is_changed = changed.contains(&item.id);
                let path = root.join(item.id.as_ref());
                let folder = item.is_folder();
                let view = view.clone();
                ListItem::new(ix)
                    .w_full()
                    .py_0p5()
                    .px_2()
                    .pl(px(12.) * entry.depth() + px(8.))
                    .selected(selected)
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(icon)
                            .child(div().truncate().child(item.label.clone()))
                            .when(is_changed, |this| this.child(div().text_xs().text_color(gpui_kit::green()).child("●"))),
                    )
                    .on_click(move |_, window, cx| {
                        if folder {
                            return;
                        }
                        let path = path.clone();
                        let _ = view.update(cx, |t, cx| t.open_file(path, 0, window, cx));
                    })
            })
            .size_full(),
        )
    }

    fn show_changed_file(&mut self, path: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        self.show_tab(|k| matches!(k, TabKind::Changes), window, cx);
        let diff = self.changes.read(cx).diff.clone();
        diff.update(cx, |state, cx| {
            state.set_file_collapsed(&path, false, cx);
            state.scroll_to_file(&path, cx);
        });
    }

    pub fn open_file(&mut self, path: PathBuf, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| match &t.kind {
            TabKind::File(f) => f.read(cx).path == path,
            _ => false,
        }) {
            self.activate(ix, window, cx);
            if let TabKind::File(file) = &self.tabs[ix].kind {
                file.update(cx, |f, cx| f.go_to_line(line, window, cx));
            }
            return;
        }

        let content = match std::fs::read(&path).map(String::from_utf8) {
            Ok(Ok(content)) => content,
            failed => {
                let reason = match failed {
                    Ok(Err(_)) => {
                        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        format!("Binary file, {}: not shown as text", human_size(size))
                    }
                    Err(e) => format!("Cannot read: {e}"),
                    Ok(Ok(_)) => unreachable!(),
                };
                let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let view = cx.new(|_| BinaryView { path: path.clone(), reason });
                let mut tab = TabItem::new(title, TabKind::Binary(view));
                tab.right = self.focus_right && self.is_split();
                self.tabs.push(tab);
                self.activate(self.tabs.len() - 1, window, cx);
                return;
            }
        };
        let language = lsp::language_for(&path);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language)
                .line_number(true)
                .indent_guides(true)
                .soft_wrap(false)
        });
        editor.update(cx, |state, cx| {
            state.set_value(content.clone(), window, cx);
            state.set_cursor_position(Position::new(line, 0), window, cx);
        });

        let saved: SharedString = content.clone().into();
        let root = self.root.clone();
        let task = cx.weak_entity();
        let file = cx.new(|cx| {
            // The editor notifies on cursor moves too: follow the line for blame.
            cx.observe(&editor, |this: &mut FileView, editor, cx| {
                let line = editor.read(cx).cursor_position().line;
                if line != this.cursor_line {
                    this.cursor_line = line;
                    cx.notify();
                }
            })
            .detach();
            cx.subscribe(&editor, |this: &mut FileView, _, event: &InputEvent, cx| {
                match event {
                InputEvent::Change => this.on_edit(cx),
                // Saved when you leave the editor, as JetBrains IDEs do.
                InputEvent::Blur => this.autosave(cx),
                _ => {}
            }})
            .detach();
            FileView {
            path: path.clone(),
            editor: editor.clone(),
            saved,
            disk_mtime: mtime(&path),
            dirty: false,
            conflict: false,
            error: None,
            lsp_version: 1,
            edit_version: 0,
            _lsp_sync: Task::ready(()),
            expand_stack: Vec::new(),
            last_expanded: None,
            root: root.clone(),
            task: task.clone(),
            blame: Rc::default(),
            cursor_line: line,
            _blame: Task::ready(()),
            lsp_status: SharedString::default(),
            lsp_ok: Some(false),
            lsp_client: None,
            lsp_program: String::new(),
            // Re-render while the server reports progress, so the header can
            // say "indexing" instead of a misleading "ready".
            _poll: cx.spawn(async move |this: WeakEntity<FileView>, cx| {
                let mut last = None;
                loop {
                    cx.background_executor().timer(Duration::from_millis(500)).await;
                    let Ok(busy) = this.update(cx, |f, _| f.lsp_client.as_ref().and_then(|c| c.busy())) else {
                        break;
                    };
                    if busy != last {
                        last = busy;
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            break;
                        }
                    }
                }
            }),
        }});
        // The tab label shows whether the file has unsaved edits.
        cx.observe(&file, |_, _, cx| cx.notify()).detach();
        file.update(cx, |f, cx| f.load_blame(cx));
        self.attach_lsp(&path, content, &editor, &file, cx);
        // No language server: definitions by search, so Ctrl+click and Ctrl+B still work.
        let search = SearchDefinitionProvider { root: self.root.clone(), files: self.shared_files.clone() };
        let task_weak = cx.weak_entity();
        let this_path = path.clone();
        editor.update(cx, |state, _| {
            let lsp = state.lsp_mut();
            if lsp.definition_provider.is_none() {
                lsp.definition_provider = Some(Rc::new(search));
                lsp.show_document = Some(show_document_handler(task_weak, this_path));
            }
        });

        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut tab = TabItem::new(title, TabKind::File(file));
        tab.right = self.focus_right && self.is_split();
        self.tabs.push(tab);
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn attach_lsp(
        &mut self,
        path: &PathBuf,
        content: String,
        editor: &Entity<EditorState>,
        file: &Entity<FileView>,
        cx: &mut Context<Self>,
    ) {
        let set_status = |file: &Entity<FileView>, ok: Option<bool>, status: String, cx: &mut App| {
            file.update(cx, |f, cx| {
                f.lsp_ok = ok;
                f.lsp_status = status.into();
                cx.notify();
            });
        };
        let (language_id, program, args) = match lsp::server_for(path) {
            Ok(server) => server,
            Err(reason) => return set_status(file, Some(false), reason, cx),
        };
        let project = lsp::project_root(program, path, &self.root);
        let key = (program, project.clone());
        let client = match self.lsp.get(&key) {
            Some(client) => client.clone(),
            None => match lsp::LspClient::spawn(program, args, &project) {
                Ok(client) => {
                    self.lsp.insert(key, client.clone());
                    client
                }
                Err(error) => {
                    // Not installed is the normal case for many languages, not an error.
                    let missing = error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound);
                    let status = if missing { format!("{program} not installed") } else { format!("{program} failed: {error:#}") };
                    return set_status(file, Some(false), status, cx);
                }
            },
        };
        set_status(file, None, "starting…".to_string(), cx);
        let shown_root = project.strip_prefix(&self.root).unwrap_or(&project).display().to_string();
        let shown_root = if shown_root.is_empty() { ".".to_string() } else { shown_root };
        file.update(cx, |f, _| {
            f.lsp_client = Some(client.clone());
            f.lsp_program = if shown_root == "." { program.to_string() } else { format!("{program} @ {shown_root}") };
        });

        let open_client = client.clone();
        let open_path = path.clone();
        let weak_file = file.downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_spawn(async move { open_client.did_open(&open_path, language_id, &content) })
                .await;
            let (ok, status) = match result {
                Ok(()) => (true, "ready · hover, Ctrl+click = definition".to_string()),
                Err(error) => (false, format!("failed: {error:#}")),
            };
            let _ = weak_file.update(cx, |f, cx| {
                f.lsp_ok = Some(ok);
                f.lsp_status = status.into();
                cx.notify();
            });
        })
        .detach();

        let bridge = Rc::new(LspBridge { client, uri: lsp::path_to_uri(path) });
        let task = cx.weak_entity();
        let this_path = path.clone();
        editor.update(cx, |state, cx| {
            let lsp = state.lsp_mut();
            lsp.hover_provider = Some(bridge.clone());
            lsp.definition_provider = Some(bridge);
            lsp.show_document = Some(show_document_handler(task, this_path));
            cx.notify();
        });
    }

    fn render_tabs(&self, right: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let shown = if right { self.right_active } else { Some(self.left_active) };
        let in_pane: Vec<usize> = (0..self.tabs.len()).filter(|&ix| self.tabs[ix].right == right).collect();
        let selected = in_pane.iter().position(|&ix| Some(self.tabs[ix].id) == shown).unwrap_or(0);
        let can_split = !right && in_pane.len() > 1;
        let map = in_pane.clone();
        TabBar::new(if right { "tabs-right" } else { "tabs-left" })
            .selected_index(selected)
            .on_click(cx.listener(move |this, pos: &usize, window, cx| {
                let Some(&ix) = map.get(*pos) else { return };
                // The tab bar takes the clicks for itself, so a double click is
                // told apart here: the same tab again within 400 ms.
                let id = this.tabs[ix].id;
                let now = std::time::Instant::now();
                let double = this.last_tab_click.is_some_and(|(last, at)| last == id && now - at < Duration::from_millis(400));
                this.last_tab_click = Some((id, now));
                this.activate(ix, window, cx);
                if double {
                    this.last_tab_click = None;
                    this.toggle_zoom(right, window, cx);
                }
            }))
            .suffix(
                h_flex()
                    .child(
                        Button::new("zoom")
                            .small()
                            .ghost()
                            .label(if self.zoomed.is_some() { "⤡" } else { "⤢" })
                            .selected(self.zoomed.is_some())
                            .tooltip("Focus mode: this pane over everything (Shift+Esc, or double-click a tab)")
                            .on_click(cx.listener(move |this, _, window, cx| this.toggle_zoom(right, window, cx))),
                    )
                    .child(
                        Button::new("new-terminal")
                            .small()
                            .ghost()
                            .label("+")
                            .tooltip("New terminal here")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.focus_right = right && this.is_split();
                                this.new_terminal(window, cx)
                            })),
                    )
                    .child(
                        Button::new("new-browser")
                            .small()
                            .ghost()
                            .label("Web")
                            .tooltip("New browser tab here")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.focus_right = right && this.is_split();
                                this.new_browser(window, cx)
                            })),
                    )
                    .when(!right, |bar| {
                        bar.child(
                            Button::new("split")
                                .small()
                                .ghost()
                                .label("◫")
                                .disabled(!can_split)
                                .tooltip("Move this tab to the right pane (or right-click any tab)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(ix) = this.ix_of(this.left_active) {
                                        this.move_to_other_pane(ix, window, cx)
                                    }
                                })),
                        )
                    })
                    .when(right, |bar| {
                        bar.child(
                            Button::new("join")
                                .small()
                                .ghost()
                                .label("▣")
                                .tooltip("Join: move these tabs back to the left pane")
                                .on_click(cx.listener(|this, _, window, cx| this.join_panes(window, cx))),
                        )
                    }),
            )
            .children(in_pane.into_iter().map(|ix| (ix, &self.tabs[ix])).map(|(ix, tab)| {
                let mut label = tab.title.clone();
                if let TabKind::Terminal(view) = &tab.kind {
                    let title = view.read(cx).state.read(cx).title();
                    if !title.is_empty() {
                        label = format!("{}: {}", tab.title, short_title(title)).into();
                    }
                }
                if tab.title == "agent" {
                    if let Some((glyph, _)) = status_glyph(&self.agent_status) {
                        label = format!("{glyph} {label}").into();
                    }
                }
                if let TabKind::Browser(view) = &tab.kind {
                    let title = view.read(cx).title();
                    if !title.is_empty() && title != "about:blank" {
                        label = short_title(&title).into();
                    }
                }
                if let TabKind::File(file) = &tab.kind {
                    if file.read(cx).dirty {
                        label = format!("● {label}").into();
                    }
                }
                // Middle click closes what the user opened, never the agent, the
                // first shell or the Changes/History views.
                let middle_closes = match &tab.kind {
                    TabKind::File(_) | TabKind::Browser(_) | TabKind::Binary(_) => true,
                    TabKind::Terminal(_) => tab.title != "agent" && tab.title != "shell",
                    _ => false,
                };
                let dragged = DraggedTab { tab_id: tab.id, title: label.clone() };
                let item = Tab::new()
                    .label(label)
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, window, cx| this.move_to_other_pane(ix, window, cx)),
                    )
                    .when(middle_closes, |tab| {
                        tab.on_mouse_down(
                            MouseButton::Middle,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, window, cx)
                            }),
                        )
                    })
                    .on_drag(dragged, |dragged, _, _, cx| {
                        TAB_DRAG.store(true, std::sync::atomic::Ordering::Relaxed);
                        cx.new(|_| dragged.clone())
                    });
                let closable = match &tab.kind {
                    TabKind::Terminal(_) => tab.title != "agent",
                    TabKind::File(_) | TabKind::Browser(_) | TabKind::Binary(_) => true,
                    TabKind::Changes | TabKind::History => true,
                };
                if closable {
                    item.suffix(
                        Button::new(("close", ix))
                            .xsmall()
                            .ghost()
                            .label("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, window, cx)
                            })),
                    )
                } else {
                    item
                }
            }))
    }

    fn render_files(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("changed-files")
            .track_focus(&self.side_focus)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| this.side_key(e, window, cx)))
            .size_full()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("pane-changes")
                            .small()
                            .ghost()
                            .label(format!("Changes ({})", self.files.len()))
                            .selected(self.right == RightPane::Changes)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.right = RightPane::Changes;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("pane-files")
                            .small()
                            .ghost()
                            .label("Files")
                            .selected(self.right == RightPane::Files)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.right = RightPane::Files;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("pane-structure")
                            .small()
                            .ghost()
                            .label("Structure")
                            .selected(self.right == RightPane::Structure)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.right = RightPane::Structure;
                                cx.notify();
                            })),
                    ),
            )
            .when(self.right == RightPane::Files, |this| this.child(self.render_tree(cx)))
            .when(self.right == RightPane::Structure, |this| this.child(self.render_structure(cx)))
            .when(self.right == RightPane::Changes, |this| {
                this.child(self.render_change_lists(cx)).child(self.render_commit_box(cx))
            })
    }

    fn render_structure(&self, cx: &mut Context<Self>) -> AnyElement {
        let file = self.structure_file.and_then(|id| {
            self.tabs.iter().find_map(|t| match &t.kind {
                TabKind::File(f) if t.id == id => Some((id, f.clone())),
                _ => None,
            })
        });
        let Some((id, file)) = file else {
            return div()
                .p_3()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Open a file to see its structure")
                .into_any_element();
        };
        let (path, cursor) = {
            let f = file.read(cx);
            (f.path.clone(), f.cursor_line)
        };
        let items = self.structure_items(cx);
        let keyboard = self.side_has_focus.get();
        // The innermost declaration the cursor is in: the last one starting at or above it.
        let current = items.iter().rposition(|i| i.line <= cursor);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        v_flex()
            .id("structure")
            .flex_1()
            .min_h_0()
            .child(div().px_3().py_1().text_xs().text_color(cx.theme().muted_foreground).child(name))
            .children(items.iter().enumerate().map(|(ix, item)| {
                let (file, line) = (file.clone(), item.line);
                div()
                    .id(("decl", ix))
                    .pl(px(12. + 14. * item.depth as f32))
                    .pr_2()
                    .py_0p5()
                    .text_sm()
                    .truncate()
                    .cursor_pointer()
                    .when(Some(ix) == current, |d| d.bg(cx.theme().accent))
                    .when(keyboard && ix == self.side_selected, |d| d.border_l_2().border_color(cx.theme().primary))
                    .hover(|d| d.bg(cx.theme().accent))
                    .child(item.label.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(ix) = this.tabs.iter().position(|t| t.id == id) {
                            this.activate(ix, window, cx);
                        }
                        file.update(cx, |f, cx| f.go_to_line(line, window, cx));
                    }))
            }))
            .overflow_y_scrollbar()
            .into_any_element()
    }

    fn staged_paths(&self) -> Vec<String> {
        self.files.iter().filter(|f| self.state_of(&f.path).staged).map(|f| f.path.to_string()).collect()
    }

    fn state_of(&self, path: &str) -> git::FileState {
        self.file_states.get(path).copied().unwrap_or(git::FileState { staged: false, unstaged: true })
    }

    fn render_commit_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let staged = self.staged_paths().len();
        let b = &self.branch_info;
        let busy = self.busy.is_some();
        v_flex()
            .flex_none()
            .gap_1p5()
            .p_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(gpui_kit::component::input::Textarea::new(&self.commit_input).small())
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("commit")
                            .small()
                            .outline()
                            .disabled(busy || self.files.is_empty())
                            .label(if staged > 0 { format!("Commit {staged}") } else { "Commit all".to_string() })
                            .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                    )
                    .child(
                        Button::new("push")
                            .small()
                            .ghost()
                            .disabled(busy)
                            .label(if b.ahead > 0 { format!("Push ↑{}", b.ahead) } else { "Push".to_string() })
                            .on_click(cx.listener(|this, _, window, cx| {
                                let upstream = this.branch_info.upstream.is_some();
                                this.run_action("Push", move |root| git::push(&root, upstream), false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("pr")
                            .small()
                            .ghost()
                            .disabled(busy)
                            .label("PR")
                            .tooltip("Open this branch's pull request, creating it if needed")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let upstream = this.branch_info.upstream.is_some();
                                this.run_action("PR", move |root| git::pull_request(&root, upstream), true, window, cx)
                            })),
                    ),
            )
            .when_some(self.busy, |this, label| {
                this.child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!("{label}…")))
            })
            .when_some(self.action.clone().filter(|_| !busy), |this, (ok, message)| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(if ok { gpui_kit::green() } else { gpui_kit::red() })
                        .child(message.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().to_string()),
                )
            })
    }

    fn render_change_lists(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let staged: Vec<usize> = (0..self.files.len()).filter(|&i| self.state_of(&self.files[i].path).staged).collect();
        let unstaged: Vec<usize> = (0..self.files.len()).filter(|&i| self.state_of(&self.files[i].path).unstaged).collect();
        let section = |id: &'static str, title: String, all_label: &'static str, stage: bool, cx: &mut Context<Self>| {
            h_flex()
                .justify_between()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(title)
                .child(
                    Button::new(id)
                        .xsmall()
                        .ghost()
                        .label(all_label)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let paths: Vec<String> = this
                                .files
                                .iter()
                                .filter(|f| {
                                    let s = this.state_of(&f.path);
                                    if stage { s.unstaged } else { s.staged }
                                })
                                .map(|f| f.path.to_string())
                                .collect();
                            this.stage_paths(paths, stage, window, cx)
                        })),
                )
        };
        let mut list = v_flex().id("changed-files-list").flex_1().min_h_0();
        if !staged.is_empty() {
            list = list.child(section("unstage-all", format!("STAGED ({})", staged.len()), "unstage all", false, cx));
            for &ix in &staged {
                list = list.child(self.render_change_row(ix, true, cx));
            }
        }
        list = list.child(section("stage-all", format!("CHANGES ({})", unstaged.len()), "stage all", true, cx));
        for &ix in &unstaged {
            list = list.child(self.render_change_row(ix, false, cx));
        }
        list.overflow_y_scrollbar()
    }

    fn render_change_row(&self, ix: usize, in_staged: bool, cx: &mut Context<Self>) -> AnyElement {
        let file = &self.files[ix];
        let path = file.path.clone();
        let stage_path = file.path.to_string();
        let open_path = self.root.join(file.path.as_ref());
        let open_line = file.first_line.saturating_sub(1);
        let deleted = matches!(file.status, DiffFileStatus::Deleted);
        let (name, dir) = match file.path.rsplit_once('/') {
            Some((dir, name)) => (name.to_string(), dir.to_string()),
            None => (file.path.to_string(), String::new()),
        };
        let status = match file.status {
            DiffFileStatus::Added => "A",
            DiffFileStatus::Deleted => "D",
            DiffFileStatus::Renamed => "R",
            _ => "M",
        };
        let id = if in_staged { ix * 2 + 1 } else { ix * 2 };
        let keyboard = self.side_has_focus.get() && self.right == RightPane::Changes && ix == self.side_selected;
        h_flex()
            .id(("file", id))
            .when(keyboard, |row| row.border_l_2().border_color(cx.theme().primary))
            .group("change-row")
            .px_3()
            .py_1()
            .gap_2()
            .text_sm()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().accent))
            .child(div().w(px(12.)).text_color(cx.theme().muted_foreground).child(status))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().child(name))
                    .child(div().truncate().text_xs().text_color(cx.theme().muted_foreground).child(dir)),
            )
            .child(div().text_xs().text_color(gpui_kit::green()).child(format!("+{}", file.additions)))
            .child(div().text_xs().text_color(gpui_kit::red()).child(format!("-{}", file.deletions)))
            .when(!deleted, |row| {
                row.child(
                    Button::new(("open", id))
                        .xsmall()
                        .ghost()
                        .label("open")
                        .tooltip("Open the file at its first change")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_file(open_path.clone(), open_line, window, cx)
                        })),
                )
            })
            .when(!in_staged, |row| {
                let discard_path = file.path.to_string();
                row.child(
                    Button::new(("discard", id))
                        .xsmall()
                        .ghost()
                        .label("↺")
                        .tooltip("Discard the changes to this file")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.confirm_discard(discard_path.clone(), window, cx)
                        })),
                )
            })
            .child(
                Button::new(("stage", id))
                    .xsmall()
                    .ghost()
                    .label(if in_staged { "−" } else { "+" })
                    .tooltip(if in_staged { "Unstage" } else { "Stage" })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.stage_paths(vec![stage_path.clone()], !in_staged, window, cx)
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.show_changed_file(path.clone(), window, cx)))
            .into_any_element()
    }

    fn confirm_discard(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let task = task.clone();
            let path = path.clone();
            dialog
                .confirm()
                .title(format!("Discard changes to {path}?"))
                .description("Staged and unstaged edits are lost; a new file is deleted.")
                .ok_text("Discard")
                .on_ok(move |_, window, cx| {
                    let path = path.clone();
                    let _ = task.update(cx, |t, cx| {
                        t.run_action("Discard", move |root| git::discard(&root, &path), false, window, cx)
                    });
                    true
                })
        });
    }

    fn stage_paths(&mut self, paths: Vec<String>, stage: bool, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let label = if stage { "Stage" } else { "Unstage" };
        self.run_action(
            label,
            move |root| if stage { git::stage(&root, &paths) } else { git::unstage(&root, &paths) },
            false,
            window,
            cx,
        );
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.commit_input.read(cx).value().trim().to_string();
        if message.is_empty() {
            self.action = Some((false, "Write a commit message first".into()));
            cx.notify();
            return;
        }
        let all = self.staged_paths().is_empty();
        self.run_action("Commit", move |root| git::commit(&root, &message, all), false, window, cx);
    }

    /// Runs a git/gh job off the UI thread, shows its outcome under the
    /// buttons, and refreshes. A successful commit clears the message.
    fn run_action(
        &mut self,
        label: &'static str,
        job: impl FnOnce(PathBuf) -> Result<String, String> + Send + 'static,
        open_url: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(label);
        self.action = None;
        cx.notify();
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { job(root) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(out) => {
                        if open_url {
                            cx.open_url(&out);
                        }
                        if label == "Commit" {
                            this.commit_input.update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        let summary = out.lines().next().unwrap_or_default().to_string();
                        this.action = Some((true, if summary.is_empty() { format!("{label}: done") } else { summary }));
                    }
                    Err(error) => this.action = Some((false, format!("{label}: {error}"))),
                }
                let _ = this.refresh_tx.try_send(());
                cx.notify();
            });
        })
        .detach();
    }
}

/// A terminal title for a tab: shells report their working directory, of
/// which the last component says enough.
fn short_title(title: &str) -> String {
    let title = title.trim();
    let tail = if title.contains('/') {
        title.trim_end_matches('/').rsplit('/').next().unwrap_or(title)
    } else {
        title
    };
    truncate(tail, 24)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

impl Render for TaskView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.side_has_focus.set(self.side_focus.is_focused(window));
        let slot = |this: &Self, right: bool, cx: &mut Context<Self>| -> AnyElement {
            if this.zoomed == Some(right) {
                div().size_full().into_any_element()
            } else {
                this.render_pane(right, cx).into_any_element()
            }
        };
        let content: AnyElement = if self.is_split() {
            h_resizable("panes")
                .child(resizable_panel().child(slot(self, false, cx)))
                .child(resizable_panel().child(slot(self, true, cx)))
                .into_any_element()
        } else {
            slot(self, false, cx)
        };
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &ToggleZoom, window, cx| this.toggle_zoom_focused(window, cx)))
            .on_action(cx.listener(|this, _: &ShowOutline, window, cx| this.show_outline(window, cx)))
            .on_action(cx.listener(|this, _: &SendToAgent, window, cx| this.send_editor_selection(window, cx)))
            .child(
                h_resizable("task")
                    .on_resize(|state, _, cx| {
                        if let Some(width) = state.read(cx).sizes().get(1).copied() {
                            cx.set_global(SidePanelWidth(width));
                        }
                    })
                    .child(resizable_panel().child(
                        v_flex()
                            .size_full()
                            .min_w_0()
                            .child(self.render_branch(cx))
                            .child(div().flex_1().min_h_0().child(content)),
                    ))
                    .child(
                        resizable_panel()
                            .size(cx.try_global::<SidePanelWidth>().map_or(px(300.), |w| w.0))
                            .size_range(px(200.)..px(640.))
                            .child(self.render_files(cx)),
                    ),
            )
            .into_any_element()
    }
}

/// Nests sorted relative paths into tree items. Folder ids end in `/`, and the
/// folders in `expanded` stay open across rebuilds.
fn build_tree(files: &[String], expanded: &HashSet<SharedString>) -> Vec<TreeItem> {
    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        files: Vec<String>,
    }
    let mut root = Node::default();
    for file in files {
        let mut node = &mut root;
        let mut parts = file.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                node.files.push(file.clone());
            } else {
                node = node.dirs.entry(part.to_string()).or_default();
            }
        }
    }
    fn items(node: &Node, prefix: &str, expanded: &HashSet<SharedString>) -> Vec<TreeItem> {
        let mut out = Vec::new();
        for (name, child) in &node.dirs {
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let id = SharedString::from(format!("{path}/"));
            let open = expanded.contains(&id);
            out.push(TreeItem::new(id, name.clone()).children(items(child, &path, expanded)).expanded(open));
        }
        for file in &node.files {
            let name = file.rsplit('/').next().unwrap_or(file);
            out.push(TreeItem::new(file.clone(), name.to_string()));
        }
        out
    }
    items(&root, "", expanded)
}

/// A glyph and its color for the agent status, if it has one to show.
pub fn status_glyph(status: &crate::agent::AgentStatus) -> Option<(&'static str, Hsla)> {
    use crate::agent::AgentStatus::*;
    Some(match status {
        Unknown => return None,
        Idle => ("○", hsla(0., 0., 0.55, 1.)),
        Working => ("●", hsla(0.58, 0.8, 0.55, 1.)),
        NeedsInput(_) => ("!", hsla(0.08, 0.9, 0.55, 1.)),
        Done => ("✓", hsla(0.33, 0.6, 0.45, 1.)),
        Exited => ("×", hsla(0., 0., 0.55, 1.)),
    })
}

/// Highlights diffs with the editor's language table, which knows more
/// (Kotlin aliases, XML, Gradle, properties) than the diff's own detection.
pub fn with_languages(files: Vec<DiffFile>) -> Vec<DiffFile> {
    files
        .into_iter()
        .map(|f| match lsp::language_for(std::path::Path::new(f.path().as_ref())) {
            "text" => f,
            language => f.with_language(language),
        })
        .collect()
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// The smallest syntax node strictly larger than `range`, by the file's
/// tree-sitter grammar.
fn syntax_parent(path: &std::path::Path, text: &str, range: std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
    let config = gpui_kit::component::highlighter::LanguageRegistry::singleton().language(lsp::language_for(path))?;
    let language = config.language?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(text, None)?;
    let mut node = tree.root_node().descendant_for_byte_range(range.start, range.end)?;
    loop {
        let r = node.byte_range();
        let larger = r.start <= range.start && r.end >= range.end && r != range;
        // Skip zero-width and whitespace-only steps: they look like no change.
        if larger && !text[r.clone()].trim().is_empty() {
            return Some(r);
        }
        node = node.parent()?;
    }
}

/// Without a grammar: the word, then the line, then everything.
fn plain_parent(text: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let line_start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[range.end..].find('\n').map_or(text.len(), |i| range.end + i);
    if range.is_empty() {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let start = text[..range.start].rfind(|c: char| !is_word(c)).map_or(0, |i| i + 1);
        let end = text[range.end..].find(|c: char| !is_word(c)).map_or(text.len(), |i| range.end + i);
        if start < end {
            return start..end;
        }
    }
    if range != (line_start..line_end) && range.start >= line_start && range.end <= line_end {
        return line_start..line_end;
    }
    0..text.len()
}

/// The line comment of a language, or the block markers wrapped around
/// lines for XML-like ones.
fn comment_markers(path: &std::path::Path) -> (&'static str, &'static str) {
    match lsp::language_for(path) {
        "go" | "rust" | "kotlin" | "java" | "swift" | "scala" | "c" | "cpp" | "csharp" | "typescript" | "tsx"
        | "javascript" | "php" | "groovy" | "zig" | "proto" | "css" => ("// ", ""),
        "sql" | "lua" => ("-- ", ""),
        "ini" => ("; ", ""),
        "batch" => ("REM ", ""),
        "xml" | "html" | "markdown" | "svelte" => ("<!-- ", " -->"),
        _ => ("# ", ""),
    }
}

/// The byte range of the selected lines and their text with comments
/// toggled: removed when every non-blank line has one, else added at the
/// common indentation.
fn toggle_comment_lines(path: &std::path::Path, text: &str, range: std::ops::Range<usize>) -> Option<(usize, usize, String)> {
    let (open, close) = comment_markers(path);
    let start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    // A selection ending at a line start does not take that line.
    let last = if range.end > range.start && text[..range.end].ends_with('\n') { range.end - 1 } else { range.end };
    let end = text[last..].find('\n').map_or(text.len(), |i| last + i);
    let lines: Vec<&str> = text[start..end].split('\n').collect();
    let open_trim = open.trim_end();
    let close_trim = close.trim_start();
    let commented = |l: &str| {
        let t = l.trim_start();
        t.starts_with(open_trim) && (close_trim.is_empty() || t.trim_end().ends_with(close_trim))
    };
    let all = lines.iter().filter(|l| !l.trim().is_empty()).all(|l| commented(l))
        && lines.iter().any(|l| !l.trim().is_empty());
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let out: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                return l.to_string();
            }
            if all {
                let lead = l.len() - l.trim_start().len();
                let mut body = &l[lead..];
                body = body.strip_prefix(open).or_else(|| body.strip_prefix(open_trim)).unwrap_or(body);
                if !close_trim.is_empty() {
                    let trimmed = body.trim_end();
                    body = trimmed.strip_suffix(close).or_else(|| trimmed.strip_suffix(close_trim)).unwrap_or(body);
                }
                format!("{}{}", &l[..lead], body)
            } else {
                format!("{}{}{}{}", &l[..indent], open, &l[indent..], close)
            }
        })
        .collect();
    Some((start, end, out.join("\n")))
}

#[cfg(test)]
mod editing_tests {
    use super::{plain_parent, toggle_comment_lines};
    use std::path::Path;

    #[test]
    fn comment_toggles_both_ways() {
        let text = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
        let a = text.find("let a").unwrap();
        let b_end = text.find("2;").unwrap() + 2;
        let (s, e, out) = toggle_comment_lines(Path::new("x.rs"), text, a..b_end).unwrap();
        assert_eq!(out, "    // let a = 1;\n    // let b = 2;");
        let commented = format!("{}{}{}", &text[..s], out, &text[e..]);
        let (s2, e2, back) = toggle_comment_lines(Path::new("x.rs"), &commented, s..s + out.len()).unwrap();
        assert_eq!(format!("{}{}{}", &commented[..s2], back, &commented[e2..]), text);
    }

    #[test]
    fn xml_wraps_lines() {
        let (_, _, out) = toggle_comment_lines(Path::new("a.xml"), "<a/>", 0..0).unwrap();
        assert_eq!(out, "<!-- <a/> -->");
    }

    #[test]
    fn plain_expansion_steps_out() {
        let text = "alpha beta\ngamma";
        assert_eq!(plain_parent(text, 7..7), 6..10);
        assert_eq!(plain_parent(text, 6..10), 0..10);
        assert_eq!(plain_parent(text, 0..10), 0..text.len());
    }
}

/// Declarations in a file, by its tree-sitter grammar: classes, functions,
/// methods, types… nested as in the source. Calls and expressions are left
/// out even when the grammar gives them a name.
fn outline(path: &std::path::Path, text: &str) -> Vec<crate::finder::OutlineItem> {
    let Some(language) = gpui_kit::component::highlighter::LanguageRegistry::singleton()
        .language(lsp::language_for(path))
        .and_then(|c| c.language)
    else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else { return Vec::new() };
    let mut items = Vec::new();
    collect_declarations(tree.root_node(), text, 0, &mut items);
    items
}

/// The first identifier within `depth` levels, breadth first: a Kotlin or
/// Java field names itself a level or two down (declarator, variable).
fn first_identifier(node: tree_sitter::Node, depth: usize) -> Option<tree_sitter::Node> {
    let mut level = vec![node];
    for _ in 0..depth {
        let mut next = Vec::new();
        for n in level {
            let mut c = n.walk();
            for child in n.named_children(&mut c) {
                if child.kind().ends_with("identifier") && child.kind() != "type_identifier" {
                    return Some(child);
                }
                next.push(child);
            }
        }
        level = next;
    }
    None
}

fn collect_declarations(node: tree_sitter::Node, text: &str, depth: usize, items: &mut Vec<crate::finder::OutlineItem>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        let declaration = ["function", "method", "class", "interface", "struct", "enum", "trait", "impl", "object", "type_spec", "type_alias", "type_item", "module", "namespace", "property", "field", "const_spec", "const_item", "static_item"]
            .iter()
            .any(|k| kind.contains(k))
            && !["call", "invocation", "reference", "expression", "parameter", "argument", "literal", "access", "body", "list", "initializer"]
                .iter()
                .any(|k| kind.contains(k))
            && !kind.ends_with("identifier")
            // A field's type or a class's body is not a declaration of its own.
            && !kind.ends_with("_type");
        let name = declaration
            .then(|| {
                child
                    .child_by_field_name("name")
                    .or_else(|| child.child_by_field_name("type"))
                    .or_else(|| first_identifier(child, 3))
            })
            .flatten()
            .and_then(|n| text.get(n.byte_range()))
            .map(|n| n.lines().next().unwrap_or(n).to_string());
        match name {
            Some(name) => {
                let short = kind
                    .trim_end_matches("_declaration")
                    .trim_end_matches("_definition")
                    .trim_end_matches("_item")
                    .trim_end_matches("_spec")
                    .replace('_', " ");
                items.push(crate::finder::OutlineItem {
                    depth,
                    label: format!("{short} {name}"),
                    line: child.start_position().row as u32,
                });
                collect_declarations(child, text, depth + 1, items);
            }
            None => collect_declarations(child, text, depth, items),
        }
    }
}

#[cfg(test)]
mod outline_tests {
    use super::outline;
    use std::path::Path;

    #[test]
    fn go_declarations() {
        let src = "package main\n\ntype Server struct{}\n\nfunc (s *Server) Run() { go s.loop() }\n\nfunc main() {}\n";
        let labels: Vec<String> = outline(Path::new("m.go"), src).into_iter().map(|i| i.label).collect();
        assert!(labels.iter().any(|l| l.ends_with("Server")), "{labels:?}");
        assert!(labels.iter().any(|l| l.ends_with("Run")), "{labels:?}");
        assert!(labels.iter().any(|l| l.ends_with("main")), "{labels:?}");
        assert!(!labels.iter().any(|l| l.contains("loop")), "{labels:?}");
    }

    #[test]
    fn fields_are_listed() {
        let ts = "class A {\n  count: number = 1;\n  name = 'x';\n  run() {}\n}\n";
        let labels: Vec<String> = outline(Path::new("a.ts"), ts).into_iter().map(|i| i.label).collect();
        assert!(labels.iter().any(|l| l.ends_with("count")), "{labels:?}");
        assert!(labels.iter().any(|l| l.ends_with("run")), "{labels:?}");
        let kt = "class A {\n    val count: Int = 1\n    fun run() {}\n}\n";
        let labels: Vec<String> = outline(Path::new("a.kt"), kt).into_iter().map(|i| i.label).collect();
        assert!(labels.iter().any(|l| l.ends_with("count")), "{labels:?}");
    }
}

/// "open" in a diff file's header: the file as it is now, at the first line
/// this diff changes in it (from `first_lines`, built from the patch shown).
pub fn open_file_button(
    root: &PathBuf,
    task: &WeakEntity<TaskView>,
    file: &DiffFile,
    first_lines: &HashMap<String, u32>,
) -> AnyElement {
    if matches!(file.status(), DiffFileStatus::Deleted) {
        return div().into_any_element();
    }
    let path = root.join(file.path().as_ref());
    let task = task.clone();
    let key = file.path().clone();
    let line = first_lines.get(key.as_ref()).copied().unwrap_or(1).saturating_sub(1);
    Button::new(SharedString::from(format!("open-{key}")))
        .xsmall()
        .ghost()
        .label("open")
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            let path = path.clone();
            let _ = task.update(cx, |t, cx| t.open_file(path, line, window, cx));
        })
        .into_any_element()
}

/// Where `symbol` is declared: a project file of that name; a class's file
/// (then `Class.member` by its outline); a class declaration; a function or
/// variable declaration. 0-based lines, with the line's code for display.
pub fn find_symbol(root: &std::path::Path, files: &[String], symbol: &str) -> Vec<(PathBuf, u32, String)> {
    let file_name_of = |f: &str| f.rsplit('/').next().unwrap_or(f).to_string();
    if let Some(file) = files.iter().find(|f| file_name_of(f) == symbol) {
        return vec![(root.join(file), 0, String::new())];
    }
    let rg = |patterns: &[String], max: u64| -> Vec<(PathBuf, u32, String)> {
        let Some(matcher) = crate::search::regexes(patterns) else { return Vec::new() };
        crate::search::search(root, &matcher, max, 200, 50)
            .into_iter()
            .map(|m| (root.join(m.path), m.line.saturating_sub(1) as u32, m.text))
            .collect()
    };
    let lowercase = symbol.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    if lowercase && !symbol.contains('.') {
        let name = symbol;
        return rg(
            &[
                format!(r"\b(fun|func|function|def|fn|val|var|const|let)\s+([A-Za-z_][\w.]*\.)?{name}\b"),
                // Go methods: func (r *T) name(
                format!(r"\bfunc\s*\([^)]*\)\s*{name}\b"),
                // Java/C-style: a type then the name then "(" at the start of a line.
                format!(r"^\s*(public|private|protected|static|final|override|suspend|internal|\s)*[\w<>\[\],.? ]+\s+{name}\s*\("),
            ],
            5,
        );
    }
    let (class, member) = symbol.split_once('.').map_or((symbol, None), |(c, m)| (c, Some(m)));
    let found = files
        .iter()
        .find(|f| file_name_of(f).split('.').next() == Some(class) && f.contains('.'))
        .map(|f| (root.join(f), 0u32, String::new()))
        .or_else(|| rg(&[format!(r"\b(class|object|interface|struct|enum|trait|type)\s+{class}\b")], 1).into_iter().next());
    let Some((path, mut line, code)) = found else { return Vec::new() };
    if let Some(member) = member {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(item) = outline(&path, &text).into_iter().find(|i| i.label.rsplit(' ').next() == Some(member)) {
                line = item.line;
            }
        }
    }
    vec![(path, line, code)]
}

/// Ctrl+click and Ctrl+B in the editor when no language server runs: the
/// word under the cursor (with `Class.` before it, if any) by `find_symbol`.
struct SearchDefinitionProvider {
    root: PathBuf,
    files: Arc<std::sync::RwLock<Vec<String>>>,
}

impl DefinitionProvider for SearchDefinitionProvider {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<lsp_types::LocationLink>>> {
        let Some(range) = text.word_range(offset) else { return Task::ready(Ok(Vec::new())) };
        let word = text.slice(range.clone()).to_string();
        // `Model.silentFail`: the member of that class, not any silentFail.
        let before = text.slice(range.start.saturating_sub(64)..range.start).to_string();
        let symbol = match before.strip_suffix('.') {
            Some(prefix) => {
                let class: String = prefix.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<Vec<_>>().into_iter().rev().collect();
                if class.chars().next().is_some_and(|c| c.is_ascii_uppercase()) { format!("{class}.{word}") } else { word.clone() }
            }
            None => word.clone(),
        };
        let origin = lsp_types::Range { start: text.offset_to_position(range.start), end: text.offset_to_position(range.end) };
        let (root, files) = (self.root.clone(), self.files.clone());
        cx.background_spawn(async move {
            let files = files.read().unwrap().clone();
            let links = find_symbol(&root, &files, &symbol)
                .into_iter()
                .filter_map(|(path, line, _)| {
                    let at = lsp_types::Range {
                        start: lsp_types::Position::new(line, 0),
                        end: lsp_types::Position::new(line, 0),
                    };
                    Some(lsp_types::LocationLink {
                        origin_selection_range: Some(origin),
                        target_uri: lsp::path_to_uri(&path).parse().ok()?,
                        target_range: at,
                        target_selection_range: at,
                    })
                })
                .collect();
            Ok(links)
        })
    }
}

/// Definitions in other files open as tabs; same-file ones fall through to
/// the editor's own cursor move.
fn show_document_handler(task: WeakEntity<TaskView>, this_path: PathBuf) -> gpui_kit::component::input::ShowDocumentHandler {
    Rc::new(move |params, window, cx| {
        let Some(target) = lsp::uri_to_path(params.uri.as_str()) else {
            return false;
        };
        if target == this_path {
            return false;
        }
        let line = params.selection.map(|r| r.start.line).unwrap_or(0);
        task.update(cx, |task, cx| task.open_file(target, line, window, cx)).is_ok()
    })
}

// ---------------------------------------------------------------- session

/// The width of the right side panel, the same in every task.
pub struct SidePanelWidth(pub Pixels);
impl Global for SidePanelWidth {}

impl TaskView {
    /// The tabs and panels, to open the same way after a restart.
    pub fn session(&self, cx: &App) -> crate::state::TaskSession {
        use crate::state::{TabSession, TaskSession};
        let rel = |path: &std::path::Path| {
            path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().into_owned()
        };
        let tabs: Vec<TabSession> = self
            .tabs
            .iter()
            .map(|tab| {
                let mut s = TabSession { right: tab.right, ..Default::default() };
                match &tab.kind {
                    // The task's first terminal: the agent, or the shell without one.
                    TabKind::Terminal(_) if tab.title == "agent" || tab.title == "shell" => s.kind = "agent".into(),
                    TabKind::Terminal(_) => s.kind = "shell".into(),
                    TabKind::Changes => s.kind = "changes".into(),
                    TabKind::History => s.kind = "history".into(),
                    TabKind::File(file) => {
                        let f = file.read(cx);
                        s.kind = "file".into();
                        s.path = Some(rel(&f.path));
                        s.line = f.cursor_line;
                    }
                    TabKind::Binary(view) => {
                        s.kind = "file".into();
                        s.path = Some(rel(&view.read(cx).path));
                    }
                    TabKind::Browser(view) => {
                        s.kind = "browser".into();
                        s.url = Some(view.read(cx).url());
                    }
                }
                s
            })
            .collect();
        let index = |id: Option<usize>| id.and_then(|id| self.ix_of(id));
        TaskSession {
            tabs,
            left_active: index(Some(self.left_active)),
            right_active: index(self.right_active),
            focus_right: self.focus_right,
            expanded: self.expanded.iter().map(|e| e.to_string()).collect(),
            side_panel: match self.right {
                RightPane::Changes => 0,
                RightPane::Files => 1,
                RightPane::Structure => 2,
            },
        }
    }

    /// Opens the tabs of a saved session next to the agent's.
    pub fn restore(&mut self, session: &crate::state::TaskSession, window: &mut Window, cx: &mut Context<Self>) {
        let mut ids: Vec<Option<usize>> = Vec::new();
        for saved in &session.tabs {
            let before: HashSet<usize> = self.tabs.iter().map(|t| t.id).collect();
            match saved.kind.as_str() {
                "agent" => {
                    ids.push(self.tabs.iter().find(|t| t.title == "agent" || t.title == "shell").map(|t| t.id));
                    continue;
                }
                "shell" => {
                    self.new_terminal_tab(window, cx);
                }
                "changes" => self.tabs.push(TabItem::new("Changes", TabKind::Changes)),
                "history" => self.tabs.push(TabItem::new("History", TabKind::History)),
                "file" => {
                    if let Some(path) = &saved.path {
                        let path = self.root.join(path);
                        if path.exists() {
                            self.open_file(path, saved.line, window, cx);
                        }
                    }
                }
                "browser" => self.open_browser(saved.url.as_deref().unwrap_or("about:blank"), window, cx),
                _ => {}
            }
            let new = self.tabs.iter_mut().find(|t| !before.contains(&t.id));
            ids.push(new.map(|tab| {
                tab.right = saved.right;
                tab.id
            }));
        }
        let pick = |ix: Option<usize>| ix.and_then(|ix| ids.get(ix).copied().flatten());
        if let Some(id) = pick(session.left_active) {
            self.left_active = id;
        }
        self.right_active = pick(session.right_active);
        self.focus_right = session.focus_right;
        self.normalize();
        self.expanded = session.expanded.iter().map(|e| SharedString::from(e.clone())).collect();
        self.right = match session.side_panel {
            1 => RightPane::Files,
            2 => RightPane::Structure,
            _ => RightPane::Changes,
        };
        self.structure_file = self.focused_ix().filter(|&ix| matches!(self.tabs[ix].kind, TabKind::File(_))).map(|ix| self.tabs[ix].id);
        self.sync_visibility(cx);
        cx.notify();
    }

    /// The agent is in the middle of a turn.
    pub fn agent_busy(&self) -> bool {
        self.agent_status == crate::agent::AgentStatus::Working
    }

    /// Stops every terminal and browser of the task before it is dropped.
    pub fn shut_down(&mut self, cx: &mut Context<Self>) {
        self.autosave_all(cx);
        for tab in &self.tabs {
            if let TabKind::Terminal(view) = &tab.kind {
                let state = view.read(cx).state.clone();
                state.update(cx, |s, cx| s.close(cx));
            }
        }
    }
}
