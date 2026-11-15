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
    diff::{Diff, DiffAnnotation, DiffEvent, DiffFile, DiffFileStatus, DiffLinePosition, DiffLineRange, DiffMode, DiffSide, DiffState},
    input::{CodeActionProvider, CompletionProvider, DefinitionProvider, DocumentRangeSemanticTokensProvider, Editor, EditorState, HoverProvider, InputEvent, Position, Rope, RopeExt as _},
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
    /// Its session in the daemon, when the program runs there.
    session: Option<String>,
    /// Numbers shells within the task, for their session after a restart.
    seq: usize,
    /// The agent's terminal: a program other than the shell has run in it,
    /// so the shell at its prompt means the agent ended.
    agent: bool,
    ran: bool,
    task: Option<WeakEntity<TaskView>>,
    /// The search bar (Ctrl+F), while open.
    find: Option<Entity<gpui_kit::component::input::InputState>>,
    /// The `.ideae.toml` service it runs, if any.
    pub service: Option<String>,
}

impl TermView {
    fn new(
        id: SharedString,
        state: Entity<TerminalState>,
        session: Option<String>,
        seq: usize,
        task: Option<WeakEntity<TaskView>>,
        service: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        // One size for every terminal: the shared one when this one starts and
        // whenever it changes, and this one's own zoom keys (Ctrl+=) set it.
        Self::follow_zoom(&state, cx);
        cx.observe_global::<crate::zoom::Zoom>(|this, cx| Self::follow_zoom(&this.state, cx)).detach();
        cx.subscribe(&state, |this, state, event: &TerminalEvent, cx| match event {
            TerminalEvent::FontSizeChanged => {
                let size = f32::from(state.read(cx).font_size(cx));
                crate::zoom::set(crate::zoom::Kind::Terminal, Some(size), cx);
            }
            TerminalEvent::ForegroundChanged => {
                if state.read(cx).foreground().is_some_and(|p| !p.is_shell()) {
                    this.ran = true;
                }
                cx.notify();
            }
            _ => {}
        })
        .detach();
        let agent = task.is_some();
        let ran = state.read(cx).foreground().is_some_and(|p| !p.is_shell());
        Self { id, state, session, seq, agent, ran, task, find: None, service }
    }

    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::input::InputState;
        let input = match &self.find {
            Some(input) => input.clone(),
            None => {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in the terminal"));
                // What is selected is what you would look for.
                if let Some(text) = self.state.read(cx).selected_text().filter(|t| !t.contains('\n')) {
                    input.update(cx, |i, cx| i.set_value(text.clone(), window, cx));
                    self.state.update(cx, |s, cx| s.find(text, cx));
                }
                cx.subscribe_in(&input, window, |this, input, event: &InputEvent, _, cx| match event {
                    InputEvent::Change => {
                        let needle = input.read(cx).value().to_string();
                        this.state.update(cx, |s, cx| s.find(needle, cx));
                    }
                    InputEvent::PressEnter { shift, .. } => {
                        let older = !*shift;
                        this.state.update(cx, |s, cx| s.find_next(older, cx));
                    }
                    _ => {}
                })
                .detach();
                self.find = Some(input.clone());
                input
            }
        };
        input.update(cx, |i, cx| i.select_all(window, cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        self.state.update(cx, |s, cx| s.find("", cx));
        self.state.read(cx).focus_handle().clone().focus(window, cx);
        cx.notify();
    }

    fn render_find(&self, input: Entity<gpui_kit::component::input::InputState>, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state.read(cx).find_state().cloned();
        let empty = input.read(cx).value().is_empty();
        let count = match state {
            _ if empty => String::new(),
            Some(f) if f.total > 0 => match f.selected {
                // Numbered from the oldest, as they read top to bottom.
                Some(ix) => format!("{} of {}", f.total - ix, f.total),
                None => format!("{} matches", f.total),
            },
            _ => "No matches".into(),
        };
        h_flex()
            .flex_none()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .capture_action(cx.listener(|this, _: &gpui_kit::component::input::Escape, window, cx| {
                cx.stop_propagation();
                this.close_find(window, cx);
            }))
            .child(div().w(px(260.)).child(gpui_kit::component::input::Input::new(&input).small()))
            .child(div().min_w(px(80.)).text_xs().text_color(cx.theme().muted_foreground).child(count))
            .child(
                Button::new("find-newer")
                    .xsmall()
                    .ghost()
                    .icon(IconName::ArrowUp)
                    .tooltip("Newer match (Shift+Enter)")
                    .on_click(cx.listener(|this, _, _, cx| this.state.update(cx, |s, cx| s.find_next(false, cx)))),
            )
            .child(
                Button::new("find-older")
                    .xsmall()
                    .ghost()
                    .icon(IconName::ArrowDown)
                    .tooltip("Older match (Enter)")
                    .on_click(cx.listener(|this, _, _, cx| this.state.update(cx, |s, cx| s.find_next(true, cx)))),
            )
            .child(div().flex_1())
            .child(
                Button::new("find-close")
                    .xsmall()
                    .ghost()
                    .label("×")
                    .tooltip("Close (Esc)")
                    .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))),
            )
            .into_any_element()
    }

    /// The agent's program is gone: its shell is back at the prompt, or the
    /// terminal itself ended.
    pub fn agent_ended(&self, cx: &App) -> bool {
        if !self.agent {
            return false;
        }
        let state = self.state.read(cx);
        Self::gone(state) || (self.ran && state.foreground().is_some_and(|p| p.is_shell()))
    }

    /// The terminal's own program, the shell, is no more.
    fn gone(state: &TerminalState) -> bool {
        use gpui_cn::terminal::TerminalStatus;
        state.is_closed() || matches!(state.status(), TerminalStatus::Exited(_) | TerminalStatus::Failed(_))
    }

    fn render_agent_ended(&self, cx: &mut Context<Self>) -> AnyElement {
        let task = self.task.clone();
        h_flex()
            .flex_none()
            .gap_2()
            .px_3()
            .py_1()
            .text_xs()
            .bg(cx.theme().muted)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_color(cx.theme().muted_foreground).child("The agent has ended"))
            .child(
                Button::new("restart-agent")
                    .xsmall()
                    .outline()
                    .label("Restart")
                    .tooltip("Start the agent again, continuing its last conversation")
                    .on_click(move |_, window, cx| {
                        if let Some(task) = task.as_ref().and_then(|t| t.upgrade()) {
                            task.update(cx, |t, cx| t.restart_agent(window, cx));
                        }
                    }),
            )            .into_any_element()
    }

    /// Stops the terminal's program, and with it everything in its session.
    pub fn end(view: &Entity<TermView>, cx: &mut App) {
        let (state, session) = {
            let v = view.read(cx);
            (v.state.clone(), v.session.clone())
        };
        state.update(cx, |s, cx| s.close(cx));
        #[cfg(unix)]
        if let (Some(session), Some(daemon)) = (session, crate::daemon::client::get()) {
            daemon.kill(&session);
        }
        #[cfg(not(unix))]
        let _ = session;
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    fn follow_zoom(state: &Entity<TerminalState>, cx: &mut App) {
        let size = crate::zoom::current(cx).terminal;
        state.update(cx, |state, cx| match size {
            Some(size) => state.set_font_size(px(size), cx),
            None => state.reset_font_size(cx),
        });
    }
}

impl Render for TermView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let highlight = cx.theme().primary.opacity(0.15);
        let ended = if self.agent_ended(cx) { Some(self.render_agent_ended(cx)) } else { None };
        let find = match self.find.clone() {
            Some(input) => Some(self.render_find(input, cx)),
            None => None,
        };
        v_flex()
            .size_full()
            .on_action(cx.listener(|this, _: &FindInTerminal, window, cx| this.open_find(window, cx)))
            .children(ended)
            .children(find)
            .child(
        div()
            .id("terminal-drop")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            // Files dropped from a file manager: their paths, typed in as text.
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(highlight))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                cx.stop_propagation();
                let text = paths.paths().iter().map(|p| shell_quote(&p.display().to_string())).collect::<Vec<_>>().join(" ");
                this.state.update(cx, |s, cx| s.paste_text(format!("{text} "), cx));
                this.state.read(cx).focus_handle().clone().focus(window, cx);
            }))
            .child(crate::zoom::wheel(crate::zoom::Kind::Terminal))
            .child(Terminal::new(self.id.clone(), &self.state)),
        )
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

actions!(ideae_file, [SaveFile, ExpandSelection, ShrinkSelection, ToggleComment, ShowOutline, ShowUsages, SendToAgent]);
actions!(ideae_edit, [DeleteLine, DuplicateLine, MoveLineUp, MoveLineDown, JoinLines, ToggleCase, NextOccurrence, GoToLine, RenameSymbol, FormatFile, Fold, Unfold, FoldAll, UnfoldAll, ParameterInfo]);
actions!(ideae_task, [ToggleZoom, SendDiffToAgent, CommentOnDiff, FindInTerminal]);

actions!(ideae_task, [NextTab, PrevTab, NavigateBack, NavigateForward]);

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
    /// The edit version being written, if a write is under way, and what
    /// orders it with a later one.
    saving: Option<u64>,
    save_seq: u64,
    write_gate: Arc<std::sync::Mutex<u64>>,
    _save: Task<()>,
    /// The task's worktree (for git) and the task, to open a blamed commit.
    root: PathBuf,
    task: WeakEntity<TaskView>,
    /// Who last changed each line, and the line the cursor is on.
    blame: Rc<Vec<Option<Arc<git::BlameLine>>>>,
    cursor_line: u32,
    /// The cursor's column, for the navigation history to come back to.
    cursor_col: u32,
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
    /// The server this file would have if it were installed.
    lsp_missing: Option<&'static str>,
    /// Markdown shown as it reads instead of its source.
    preview: bool,
    /// The text whose next occurrence was added to the selection last,
    /// and where that one ends.
    occurrence: Option<(String, usize)>,
    /// The file as the last commit has it, for the gutter's marks of what
    /// changed since; None for a file that commit lacks.
    committed: Option<Arc<str>>,
    _marks: Task<()>,
    /// The count of the language server's reports when its problems were
    /// last put into the editor.
    problems_seen: u64,
    /// The signature shown above the cursor: its text, the parameter being
    /// written, the line it was asked on.
    signature: Option<(String, Option<std::ops::Range<usize>>, u32)>,
    _signature: Task<()>,
    _poll: Task<()>,
}

/// How many 150 ms waits a task out of view lets pass before it refreshes.
const HIDDEN_REFRESH_WAITS: usize = 20;

fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Whether the editor holds exactly `text`, without copying it out.
fn same_text(rope: &Rope, text: &str) -> bool {
    let mut rest = text.as_bytes();
    rope.len() == rest.len()
        && rope.chunks().all(|chunk| {
            let (head, tail) = rest.split_at(chunk.len());
            rest = tail;
            head == chunk.as_bytes()
        })
}

/// Writes of one file, in the order they were asked for: a write that
/// starts after a later one has been made is dropped. Gives the file's
/// time once written.
fn write_in_order(gate: &std::sync::Mutex<u64>, seq: u64, path: &std::path::Path, bytes: &[u8]) -> std::io::Result<Option<std::time::SystemTime>> {
    let mut latest = gate.lock().unwrap();
    if seq > *latest {
        std::fs::write(path, bytes)?;
        *latest = seq;
    }
    Ok(mtime(path))
}

/// A file found changed on disk: the time the view knew, the time now,
/// and what is in it.
struct DiskChange {
    known: Option<std::time::SystemTime>,
    now: std::time::SystemTime,
    content: String,
}

/// Off the UI thread: of the open files, those whose time on disk is not
/// the one known, read.
fn changed_on_disk(known: Vec<(PathBuf, Option<std::time::SystemTime>)>) -> Vec<Option<DiskChange>> {
    known
        .into_iter()
        .map(|(path, known)| {
            let now = mtime(&path).filter(|now| Some(*now) != known)?;
            Some(DiskChange { known, now, content: std::fs::read_to_string(&path).ok()? })
        })
        .collect()
}

impl FileView {
    /// The gutter's marks of what differs from the last commit: green by
    /// the lines added, blue by those changed, red where some are gone.
    /// After `typing`, once it pauses.
    fn mark_changes(&mut self, typing: bool, cx: &mut Context<Self>) {
        use gpui_kit::base::input::LineMarks;
        let Some(committed) = self.committed.clone() else {
            // Nothing to compare with: git ignores it, or there is no repository.
            self.editor.update(cx, |state, cx| state.set_line_marks(LineMarks::default(), cx));
            return;
        };
        self._marks = cx.spawn(async move |this, cx| {
            if typing {
                cx.background_executor().timer(Duration::from_millis(250)).await;
            }
            // Taken once typing pauses, not on every key: it is the whole file.
            let Ok(text) = this.update(cx, |this, cx| this.editor.read(cx).value()) else { return };
            let changes = cx.background_spawn(async move { crate::edit::line_changes(&committed, &text) }).await;
            let _ = this.update(cx, |this, cx| {
                let (added, changed, gone) = (hsla(0.33, 0.6, 0.45, 1.), hsla(0.58, 0.7, 0.55, 1.), hsla(0.0, 0.7, 0.55, 1.));
                let mut marks = LineMarks::default();
                marks.rows.extend(changes.added.into_iter().map(|row| (row, added)));
                marks.rows.extend(changes.changed.into_iter().map(|row| (row, changed)));
                marks.gone_before.extend(changes.gone_before.into_iter().map(|row| (row, gone)));
                this.editor.update(cx, |state, cx| state.set_line_marks(marks, cx));
            });
        });
    }

    fn on_edit(&mut self, cx: &mut Context<Self>) {
        self.edit_version += 1;
        self.mark_changes(true, cx);
        let typed = {
            let state = self.editor.read(cx);
            let (text, cursor) = (state.text(), state.cursor());
            // The character before the cursor starts one to four bytes back.
            (1..=4).find_map(|back| text.char_at(cursor.checked_sub(back)?))
        };
        if self.signature.is_some() || matches!(typed, Some('(' | ',')) {
            self.parameter_info(cx);
        }
        // Told on every change, not only when it turns dirty: the Structure
        // panel follows the text.
        self.dirty = !same_text(self.editor.read(cx).text(), &self.saved);
        cx.notify();
        // Tell the language server after typing pauses, so hover and
        // definitions see the edited text. The text is taken then, not on
        // every key: it is a copy of the whole file.
        let Some(client) = self.lsp_client.clone() else { return };
        self.lsp_version += 1;
        let path = self.path.clone();
        self._lsp_sync = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let Ok((version, text)) = this.update(cx, |f, cx| (f.lsp_version, f.editor.read(cx).value())) else { return };
            cx.background_spawn(crate::procs::on_thread(move || {
                let _ = client.did_change(&path, version, &text);
            }))
            .await;
        });
    }

    /// Saves edits unless the file changed on disk meanwhile: then the
    /// conflict bar asks, rather than silently overwriting the other side.
    pub fn autosave(&mut self, cx: &mut Context<Self>) {
        if self.dirty && !self.conflict && self.saving != Some(self.edit_version) {
            self.save(cx);
        }
    }

    /// Writes off the UI thread; the file counts as saved once it is written.
    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value();
        self.save_seq += 1;
        self.saving = Some(self.edit_version);
        let (seq, path, gate) = (self.save_seq, self.path.clone(), self.write_gate.clone());
        self._save = cx.spawn(async move |this, cx| {
            let bytes = text.clone();
            let written = cx.background_spawn(async move { write_in_order(&gate, seq, &path, bytes.as_bytes()) }).await;
            let _ = this.update(cx, |this, cx| {
                if seq == this.save_seq {
                    this.saving = None;
                    this.written(text, written, cx);
                }
            });
        });
    }

    /// The task closes or the app quits: what is unsaved, or still being
    /// written, is on disk when this returns.
    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        if self.saving.is_none() && !(self.dirty && !self.conflict) {
            return;
        }
        let text = self.editor.read(cx).value();
        self.save_seq += 1;
        self.saving = None;
        self._save = Task::ready(());
        let written = write_in_order(&self.write_gate, self.save_seq, &self.path, text.as_bytes());
        self.written(text, written, cx);
    }

    fn written(&mut self, text: SharedString, written: std::io::Result<Option<std::time::SystemTime>>, cx: &mut Context<Self>) {
        match written {
            Ok(mtime) => {
                self.dirty = !same_text(self.editor.read(cx).text(), &text);
                self.saved = text;
                self.disk_mtime = mtime;
                self.conflict = false;
                self.error = None;
                self.load_blame(cx);
                if let Some(client) = self.lsp_client.clone() {
                    let path = self.path.clone();
                    cx.background_spawn(crate::procs::on_thread(move || {
                        let _ = client.did_save(&path);
                    }))
                    .detach();
                }
            }
            Err(e) => self.error = Some(format!("Save failed: {e}")),
        }
        cx.notify();
    }

    /// The file's time as this view knows it, for the refresh to compare
    /// with the disk off the UI thread. None while a save is being written.
    fn disk_known(&self) -> Option<(PathBuf, Option<std::time::SystemTime>)> {
        self.saving.is_none().then(|| (self.path.clone(), self.disk_mtime))
    }

    /// Called on every worktree refresh with what `changed_on_disk` read:
    /// picks up changes made on disk (by the agent, git, another editor).
    /// Clean buffers reload in place; edited ones get a choice instead of
    /// losing either side.
    fn apply_disk(&mut self, change: DiskChange, window: &mut Window, cx: &mut Context<Self>) {
        // Saved or reloaded since the disk was read: the next refresh looks again.
        if self.saving.is_some() || self.disk_mtime != change.known {
            return;
        }
        self.disk_mtime = Some(change.now);
        if change.content.as_str() == self.saved.as_ref() {
            return;
        }
        if self.dirty {
            self.conflict = true;
            cx.notify();
        } else {
            self.reload(change.content, window, cx);
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
            cx.background_spawn(crate::procs::on_thread(move || {
                let _ = client.did_change(&path, version, &content_for_lsp);
            }))
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
            let since_base = crate::state::settings().gutter_since_base;
            let (blame, committed) =
                cx.background_spawn(async move { (git::blame(&root, &path), git::committed_text(&root, &path, since_base)) }).await;
            let _ = this.update(cx, |this, cx| {
                this.blame = Rc::new(blame);
                this.committed = committed.map(Arc::from);
                this.mark_changes(false, cx);
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

    /// One edit of the text, undone as one: what `change` makes of the
    /// text and the selection.
    fn edit(&mut self, window: &mut Window, cx: &mut Context<Self>, change: impl FnOnce(&str, std::ops::Range<usize>) -> Option<crate::edit::Edit>) {
        let (text, selection) = {
            let state = self.editor.read(cx);
            (state.text().to_string(), state.selected_range())
        };
        let Some(edit) = change(&text, selection) else { return };
        self.editor.update(cx, |state, cx| {
            state.set_selected_range(edit.range, cx);
            state.replace(edit.text, window, cx);
            state.set_selected_range(edit.select, cx);
        });
    }

    /// The word under the cursor, then with every press one more place
    /// where the selected text is: all of them are typed over at once.
    fn select_next_occurrence(&mut self, cx: &mut Context<Self>) {
        let (text, selection) = {
            let state = self.editor.read(cx);
            (state.text().to_string(), state.selected_range())
        };
        if selection.is_empty() {
            self.occurrence = None;
            if let Some(word) = crate::edit::word_at(&text, selection.start) {
                self.editor.update(cx, |state, cx| state.set_selected_range(word, cx));
            }
            return;
        }
        let needle = text[selection.clone()].to_string();
        // After the one added last, while the same text is being looked for.
        let from = self.occurrence.as_ref().filter(|(of, _)| *of == needle).map_or(selection.end, |(_, end)| *end);
        if let Some(found) = crate::edit::next_occurrence(&text, &needle, from, selection.start) {
            self.occurrence = Some((needle, found.end));
            self.editor.update(cx, |state, cx| state.add_selection(found, cx));
        }
    }

    /// Ctrl+P, and by itself after `(` and `,`: the signature of the call
    /// being written, from the language server.
    fn parameter_info(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.lsp_client.clone() else { return };
        let (path, text, pos) = {
            let state = self.editor.read(cx);
            (self.path.clone(), state.value().to_string(), state.cursor_position())
        };
        self._signature = cx.spawn(async move |this, cx| {
            let uri = lsp::path_to_uri(&path);
            let help = cx
                .background_spawn(crate::procs::on_thread(move || {
                    client.did_change(&path, 0, &text)?;
                    client.signature_help(&uri, pos)
                }))
                .await;
            let shown = help.ok().flatten().and_then(|help| lsp::signature_shown(&help)).map(|(label, active)| (label, active, pos.line));
            let _ = this.update(cx, |file, cx| {
                file.signature = shown;
                cx.notify();
            });
        });
    }

    /// The signature above the cursor, while the cursor is on the line it
    /// was asked for.
    fn render_signature(&self, cx: &App) -> Option<AnyElement> {
        let (label, active, line) = self.signature.as_ref()?;
        let state = self.editor.read(cx);
        if state.cursor_position().line != *line {
            return None;
        }
        let cursor = state.cursor();
        let text = state.value();
        let before = text.get(..cursor).and_then(|t| t.char_indices().next_back()).map_or(cursor, |(at, _)| at);
        let at = state.range_to_bounds(&(before..cursor)).or_else(|| state.range_to_bounds(&(cursor..cursor)))?;
        let strong = HighlightStyle { font_weight: Some(FontWeight::BOLD), color: Some(cx.theme().primary), ..Default::default() };
        let highlights: Vec<_> = active.clone().map(|range| (range, strong)).into_iter().collect();
        Some(
            deferred(
                anchored().position(at.origin).anchor(gpui_kit::Anchor::BottomLeft).snap_to_window().child(
                    div()
                        .mb_1()
                        .px_2()
                        .py_1()
                        .max_w(px(720.))
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().popover)
                        .shadow_md()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_sm()
                        .child(StyledText::new(label.clone()).with_highlights(highlights)),
                ),
            )
            .into_any_element(),
        )
    }

    fn fold(&mut self, fold: bool, all: bool, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            state.fold(fold, all, cx);
        });
    }

    /// The language server's latest word on what is wrong in the file, into
    /// the editor: underlined there, with the fixes it offers.
    fn show_problems(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.lsp_client else { return };
        let (count, found) = client.problems(&lsp::path_to_uri(&self.path));
        if count == self.problems_seen {
            return;
        }
        self.problems_seen = count;
        self.editor.update(cx, |state, cx| {
            let text = state.text().clone();
            if let Some(shown) = state.diagnostics_mut() {
                shown.reset(&text);
                shown.extend(found);
            }
            cx.notify();
        });
    }

    /// Every edit of a workspace edit: those of this file in the editor,
    /// undone there as one; those of others on disk, where their tabs pick
    /// them up. How many files changed.
    fn apply_workspace_edit(&mut self, edit: &lsp_types::WorkspaceEdit, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let mut files = 0;
        for (path, edits) in lsp::edits_by_file(edit) {
            if path == self.path {
                let before = files;
                self.edit(window, cx, |text, selection| {
                    let rewritten = crate::edit::rewrite(text, &lsp::apply_edits(text, &edits), selection.start);
                    files += usize::from(rewritten.is_some());
                    rewritten
                });
                debug_assert!(files >= before);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                let new = lsp::apply_edits(&text, &edits);
                files += usize::from(new != text && std::fs::write(&path, new).is_ok());
            }
        }
        files
    }

    /// The server is told the text as it is now, then asked: the answer
    /// comes back to `then`, in the window.
    fn ask_server<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        ask: impl FnOnce(&lsp::LspClient, &str, lsp_types::Position) -> anyhow::Result<T> + Send + 'static,
        then: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let Some(client) = self.lsp_client.clone() else {
            window.push_notification(Notification::info("No language server runs for this file."), cx);
            return;
        };
        let (path, text, pos) = {
            let state = self.editor.read(cx);
            (self.path.clone(), state.value().to_string(), state.cursor_position())
        };
        cx.spawn_in(window, async move |this, cx| {
            let uri = lsp::path_to_uri(&path);
            let answer = cx
                .background_spawn(crate::procs::on_thread(move || {
                    client.did_change(&path, 0, &text)?;
                    ask(&client, &uri, pos)
                }))
                .await;
            let _ = this.update_in(cx, |this, window, cx| match answer {
                Ok(answer) => then(this, answer, window, cx),
                Err(e) => window.push_notification(Notification::error(format!("{e:#}")).title("Language server"), cx),
            });
        })
        .detach();
    }

    /// Formats the file as its language server would.
    fn format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_server(window, cx, |client, uri, _| client.formatting(uri, 4, true), |this, edits, window, cx| {
            this.edit(window, cx, |text, selection| crate::edit::rewrite(text, &lsp::apply_edits(text, &edits), selection.start));
        });
    }

    /// Asks for a new name of the symbol under the cursor and gives it that
    /// name everywhere the language server knows it.
    fn rename_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let old = {
            let state = self.editor.read(cx);
            let text = state.text().to_string();
            crate::edit::word_at(&text, state.selected_range().start).map(|word| text[word].to_string()).unwrap_or_default()
        };
        if old.is_empty() {
            return;
        }
        let input = cx.new(|cx| gpui_kit::component::input::InputState::new(window, cx));
        input.update(cx, |input, cx| {
            input.set_value(old.clone(), window, cx);
            input.select_all(window, cx);
        });
        cx.subscribe_in(&input, window, move |this, input, event: &InputEvent, window, cx| {
            let InputEvent::PressEnter { .. } = event else { return };
            let name = input.read(cx).value().trim().to_string();
            window.close_dialog(cx);
            if name.is_empty() || name == old {
                return;
            }
            this.ask_server(
                window,
                cx,
                move |client, uri, pos| client.rename(uri, pos, &name),
                |this, edit, window, cx| {
                    let files = edit.map_or(0, |edit| this.apply_workspace_edit(&edit, window, cx));
                    let told = match files {
                        0 => "Nothing to rename here.".to_string(),
                        1 => "Renamed in 1 file.".to_string(),
                        n => format!("Renamed in {n} files."),
                    };
                    window.push_notification(Notification::info(told), cx);
                },
            );
        })
        .detach();
        let focused = Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, focused) = (input.clone(), focused.clone());
            dialog.title("Rename").w(px(420.)).content(move |content, window, cx| {
                if !focused.replace(true) {
                    let input = input.clone();
                    window.defer(cx, move |window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
                }
                content.child(gpui_kit::component::input::Input::new(&input))
            })
        });
    }

    /// Asks for a line, `120` or `120:8`, and puts the cursor there.
    fn go_to_line_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lines = self.editor.read(cx).text().to_string().lines().count().max(1);
        let input = cx.new(|cx| gpui_kit::component::input::InputState::new(window, cx).placeholder(format!("Line, or line:column, of {lines}")));
        cx.subscribe_in(&input, window, |this, input, event: &InputEvent, window, cx| {
            let InputEvent::PressEnter { .. } = event else { return };
            let typed = input.read(cx).value().trim().to_string();
            let (line, column) = typed.split_once(':').unwrap_or((typed.as_str(), "1"));
            if let Ok(line) = line.trim().parse::<u32>() {
                window.close_dialog(cx);
                this.go_to_line(line.saturating_sub(1), column.trim().parse::<u32>().unwrap_or(1).saturating_sub(1), window, cx);
            }
        })
        .detach();
        let focused = Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, focused) = (input.clone(), focused.clone());
            dialog.title("Go to line").w(px(360.)).content(move |content, window, cx| {
                if !focused.replace(true) {
                    let input = input.clone();
                    window.defer(cx, move |window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
                }
                content.child(gpui_kit::component::input::Input::new(&input))
            })
        });
    }

    fn go_to_line(&mut self, line: u32, col: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            state.jump_to_position(Position::new(line, col), JUMP_AT, window, cx);
        });
        self.editor.read(cx).focus_handle(cx).focus(window, cx);
    }
}

impl FileView {
    fn is_busy(&self) -> bool {
        self.lsp_client.as_ref().is_some_and(|c| c.busy().is_some())
    }

    fn is_markdown(&self) -> bool {
        self.path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdx"))
    }

    /// The text as it is in the editor, unsaved edits too, drawn as
    /// Markdown. A link to another file of the worktree opens it here.
    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::text::TextView;
        let (task, dir) = (self.task.clone(), self.path.parent().map(PathBuf::from).unwrap_or_default());
        div().id("markdown-preview").flex_1().min_h_0().bg(cx.theme().background).child(
            TextView::markdown("markdown", self.editor.read(cx).value())
                .scrollable(true)
                .selectable(true)
                .on_link_click(move |link, _, window, cx| {
                    let (target, _) = link.split_once('#').unwrap_or((link.as_ref(), ""));
                    let file = dir.join(target);
                    if !target.is_empty() && !target.contains("://") && file.is_file() {
                        let _ = task.update(cx, |t, cx| t.open_file(file, 0, window, cx));
                    } else if link.contains("://") {
                        cx.open_url(link);
                    }
                })
                .size_full()
                .px_6()
                .py_4(),
        )
    }

    /// The server runs, but its import of the build failed.
    fn lsp_problem(&self) -> Option<String> {
        self.lsp_client.as_ref().and_then(|c| c.problem())
    }

    fn status_text(&self) -> String {
        let busy = self.lsp_client.as_ref().and_then(|c| c.busy());
        match (self.lsp_program.is_empty(), self.lsp_ok, busy) {
            (true, _, _) => self.lsp_status.to_string(),
            (false, Some(true), Some(work)) => format!("{}: {work}…", self.lsp_program),
            (false, Some(true), None) if self.lsp_problem().is_some() => {
                let problem = self.lsp_problem().unwrap_or_default();
                let short: String = problem.chars().take(90).collect();
                format!("{}: {short}{}", self.lsp_program, if short.len() < problem.len() { "…" } else { "" })
            }
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
            .on_action(cx.listener(|this, _: &DeleteLine, window, cx| this.edit(window, cx, |text, selection| Some(crate::edit::delete_lines(text, selection)))))
            .on_action(cx.listener(|this, _: &DuplicateLine, window, cx| this.edit(window, cx, |text, selection| Some(crate::edit::duplicate(text, selection)))))
            .on_action(cx.listener(|this, _: &MoveLineUp, window, cx| this.edit(window, cx, |text, selection| crate::edit::move_lines(text, selection, true))))
            .on_action(cx.listener(|this, _: &MoveLineDown, window, cx| this.edit(window, cx, |text, selection| crate::edit::move_lines(text, selection, false))))
            .on_action(cx.listener(|this, _: &JoinLines, window, cx| this.edit(window, cx, crate::edit::join_lines)))
            .on_action(cx.listener(|this, _: &ToggleCase, window, cx| this.edit(window, cx, crate::edit::toggle_case)))
            .on_action(cx.listener(|this, _: &NextOccurrence, _, cx| this.select_next_occurrence(cx)))
            .on_action(cx.listener(|this, _: &GoToLine, window, cx| this.go_to_line_dialog(window, cx)))
            .on_action(cx.listener(|this, _: &RenameSymbol, window, cx| this.rename_dialog(window, cx)))
            .on_action(cx.listener(|this, _: &FormatFile, window, cx| this.format(window, cx)))
            .on_action(cx.listener(|this, _: &ParameterInfo, _, cx| this.parameter_info(cx)))
            .on_action(cx.listener(|this, _: &Fold, _, cx| this.fold(true, false, cx)))
            .on_action(cx.listener(|this, _: &Unfold, _, cx| this.fold(false, false, cx)))
            .on_action(cx.listener(|this, _: &FoldAll, _, cx| this.fold(true, true, cx)))
            .on_action(cx.listener(|this, _: &UnfoldAll, _, cx| this.fold(false, true, cx)))
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
                    .child(
                        div()
                            .flex_none()
                            .max_w(relative(0.45))
                            .truncate()
                            .child(self.path.strip_prefix(&self.root).unwrap_or(&self.path).display().to_string()),
                    )
                    .children(self.render_blame(cx))
                    .when(self.is_markdown(), |header| {
                        header.child(
                            Button::new("preview")
                                .xsmall()
                                .ghost()
                                .label("Preview")
                                .selected(self.preview)
                                .tooltip("The Markdown as it reads, or its source")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.preview = !this.preview;
                                    if !this.preview {
                                        this.editor.read(cx).focus_handle(cx).focus(window, cx);
                                    }
                                    cx.notify();
                                })),
                        )
                    })
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
                            .id("lsp-status")
                            .flex_none()
                            .when(self.lsp_ok == Some(true) && !self.is_busy(), |s| s.text_color(gpui_kit::green()))
                            .when(self.lsp_ok == Some(false) && self.lsp_status.contains("failed"), |s| {
                                s.text_color(gpui_kit::red())
                            })
                            .when_some(self.lsp_problem().filter(|_| !self.is_busy()), |s, problem| {
                                s.text_color(gpui_kit::red()).tooltip(move |window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(problem.clone()).build(window, cx)
                                })
                            })
                            .child(if self.lsp_missing.is_some_and(crate::lsp_install::is_installing) {
                                format!("installing {}…", self.lsp_missing.unwrap_or_default())
                            } else {
                                self.status_text()
                            }),
                    )
                    .when_some(self.lsp_missing.filter(|p| !crate::lsp_install::is_installing(p)), |header, program| {
                        let task = self.task.clone();
                        header.when(crate::lsp_install::recipe(program).is_some(), |header| {
                            header.child(Button::new("lsp-install").xsmall().ghost().label("Install…").on_click(
                                move |_, window, cx| {
                                    let _ = task.update(cx, |t, cx| t.confirm_lsp_install(program, window, cx));
                                },
                            ))
                        })
                    }),
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
            .when(self.preview, |this| this.child(self.render_preview(cx)))
            .when(!self.preview, |this| this.child(
                div().relative().flex_1().min_h_0().children(self.render_signature(cx)).child(crate::zoom::wheel(crate::zoom::Kind::Editor)).child(
                    Editor::new(&self.editor)
                        // Replaces the built-in menu, so its items are listed again.
                        .context_menu(|menu, _, _| {
                            use gpui_kit::component::input::{Copy, Cut, GoToDefinition, Paste, SelectAll};
                            // Ctrl+B on a declaration lists its usages.
                            menu.menu("Go to Definition / Usages", Box::new(GoToDefinition))
                                .menu("Implementations and Usages", Box::new(ShowUsages))
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
                        .text_size(crate::zoom::size(crate::zoom::Kind::Editor, cx)),
                ),
            ))
    }
}

/// Bridges the editor's hover and definition hooks to a language server.
struct LspBridge {
    client: Arc<lsp::LspClient>,
    uri: String,
    path: PathBuf,
    file: WeakEntity<FileView>,
}

impl CompletionProvider for LspBridge {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: lsp_types::CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        let pos = text.offset_to_position(offset);
        // The rope is shared, not copied: asked on every letter, and the
        // text is made of it where the server is told.
        let (client, uri, path, text) = (self.client.clone(), self.uri.clone(), self.path.clone(), text.clone());
        cx.spawn(async move |cx| {
            // The next letter drops this before it has cost anything.
            cx.background_executor().timer(Duration::from_millis(60)).await;
            cx.background_spawn(crate::procs::on_thread(move || {
                // The server answers about the text it has: this one, typed a moment ago.
                client.did_change(&path, 0, &text.to_string())?;
                client.completion(&uri, pos, trigger.trigger_character)
            }))
            .await
        })
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        let Some(last) = new_text.chars().last() else { return false };
        last.is_alphanumeric() || last == '_' || self.client.completion_triggers().iter().any(|t| new_text.ends_with(t.as_str()))
    }
}

impl DocumentRangeSemanticTokensProvider for LspBridge {
    fn legend(&self) -> lsp_types::SemanticTokensLegend {
        let token_types = self.client.semantic_legend().into_iter().map(lsp_types::SemanticTokenType::from).collect();
        lsp_types::SemanticTokensLegend { token_types, token_modifiers: Vec::new() }
    }

    fn semantic_tokens(
        &self,
        text: &Rope,
        range: std::ops::Range<usize>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<lsp_types::SemanticTokens>> {
        let range = lsp_types::Range::new(text.offset_to_position(range.start), text.offset_to_position(range.end));
        let (client, uri, path, text) = (self.client.clone(), self.uri.clone(), self.path.clone(), text.clone());
        cx.background_spawn(crate::procs::on_thread(move || {
            client.did_change(&path, 0, &text.to_string())?;
            let tokens = client.semantic_tokens(&uri, range);
            if std::env::var_os("IDEAE_DEBUG").is_some() {
                eprintln!("semantic tokens {uri} -> {:?}", tokens.as_ref().map(|t| t.data.len()));
            }
            tokens
        }))
    }
}

impl CodeActionProvider for LspBridge {
    fn id(&self) -> SharedString {
        "lsp".into()
    }

    fn code_actions(
        &self,
        state: Entity<EditorState>,
        range: std::ops::Range<usize>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<lsp_types::CodeAction>>> {
        let (text, range) = {
            let text = state.read(cx).text();
            (text.clone(), lsp_types::Range::new(text.offset_to_position(range.start), text.offset_to_position(range.end)))
        };
        let (client, uri, path) = (self.client.clone(), self.uri.clone(), self.path.clone());
        cx.background_spawn(crate::procs::on_thread(move || {
            client.did_change(&path, 0, &text.to_string())?;
            client.code_actions(&uri, range)
        }))
    }

    fn perform_code_action(
        &self,
        _: Entity<EditorState>,
        action: lsp_types::CodeAction,
        _: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let (client, file) = (self.client.clone(), self.file.clone());
        window.spawn(cx, async move |cx| {
            let action = match action.edit {
                Some(_) => action,
                None => cx.background_spawn(crate::procs::on_thread(move || client.resolve_code_action(&action))).await?,
            };
            let Some(edit) = action.edit else { return Ok(()) };
            file.update_in(cx, |file, window, cx| {
                file.apply_workspace_edit(&edit, window, cx);
            })
        })
    }
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
        cx.background_spawn(crate::procs::on_thread(move || {
            let result = client.hover(&uri, pos).inspect_err(|e| eprintln!("hover: {e:#}"));
            if std::env::var_os("IDEAE_DEBUG").is_some() {
                eprintln!("hover {uri} {pos:?} -> {}", if matches!(result, Ok(Some(_))) { "some" } else { "none/err" });
            }
            result
        }))
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
        cx.background_spawn(crate::procs::on_thread(move || {
            let result = client.definition(&uri, pos).inspect_err(|e| eprintln!("definition: {e:#}"));
            if std::env::var_os("IDEAE_DEBUG").is_some() {
                eprintln!("definition {uri} {pos:?} -> {result:?}");
            }
            result
        }))
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
    /// The single changes of the diff shown: each can be taken back or
    /// staged without the rest of its file.
    hunks: Rc<Vec<git::Hunk>>,
    /// Those of them that are staged, by file and lines.
    staged: Rc<HashSet<(String, String)>>,
    selection: Option<DiffLineRange>,
    task: WeakEntity<TaskView>,
    /// The branches to compare with, for the menu: listed off the UI thread.
    branches: Rc<std::cell::RefCell<Vec<String>>>,
    /// Review comments on lines of the diff, sent to the agent as one message.
    review: Vec<crate::state::ReviewNote>,
    /// The text of the last selection: a click in the context menu clears
    /// the selection before its action runs.
    selected_code: String,
}

/// Lists the repository's branches, newest first, into `known`.
fn load_branches<T: 'static>(root: PathBuf, known: Rc<std::cell::RefCell<Vec<String>>>, cx: &mut Context<T>) {
    cx.spawn(async move |_, cx| {
        let found = cx.background_spawn(async move { git::branches(&root).into_iter().take(40).collect::<Vec<_>>() }).await;
        *known.borrow_mut() = found;
    })
    .detach();
}

impl ChangesView {
    fn new(root: PathBuf, task: WeakEntity<TaskView>, cx: &mut Context<Self>) -> Self {
        let diff = cx.new(|cx| DiffState::new([], cx).with_mode(DiffMode::Split));
        cx.observe(&diff, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&diff, |this, _, event: &DiffEvent, cx| match event {
            DiffEvent::SelectionEnded(range) => {
                this.selection = Some(range.clone());
                this.selected_code = this.diff.read(cx).selected_text(cx);
                cx.notify();
            }
            // Not cleared on SelectionChanged(None): pressing the Open button
            // clears the diff selection, which would remove the button mid-click.
            _ => {}
        })
        .detach();
        let branches = Rc::default();
        load_branches(root.clone(), Rc::clone(&branches), cx);
        Self {
            root,
            diff,
            scope: DiffScope::Uncommitted,
            counts: (0, 0),
            base: None,
            first_lines: Rc::default(),
            selection: None,
            task,
            review: Vec::new(),
            selected_code: String::new(),
            branches,
            hunks: Rc::default(),
            staged: Rc::default(),
        }
    }

    /// The selected lines: where they are, their text, their language.
    fn selected(&self, cx: &App) -> Option<(String, String, &'static str)> {
        let range = self.selected_range(cx)?;
        let mut code = self.diff.read(cx).selected_text(cx);
        if code.is_empty() {
            code = self.selected_code.clone();
        }
        if code.is_empty() {
            return None;
        }
        Some((location_of(&range), code, lsp::language_for(std::path::Path::new(range.path().as_ref()))))
    }

    fn selected_range(&self, cx: &App) -> Option<DiffLineRange> {
        self.diff.read(cx).selected_lines().or(self.selection.clone())
    }

    /// A review note for `range`: the selected text when that is the range,
    /// else the lines read from the file (the changed side).
    fn note_for(&self, range: &DiffLineRange, cx: &App) -> crate::state::ReviewNote {
        let modified = range.end_side() == DiffSide::Modified;
        let mut code = String::new();
        if self.selected_range(cx).as_ref() == Some(range) {
            code = self.diff.read(cx).selected_text(cx);
            if code.is_empty() {
                code = self.selected_code.clone();
            }
        }
        if code.is_empty() && modified {
            if let Ok(text) = std::fs::read_to_string(self.root.join(range.path().as_ref())) {
                let (a, b) = (range.start().min(range.end()), range.start().max(range.end()));
                code = text.lines().skip(a.saturating_sub(1)).take(b + 1 - a.max(1)).collect::<Vec<_>>().join("\n");
            }
        }
        crate::state::ReviewNote {
            location: location_of(range),
            code,
            language: lsp::language_for(std::path::Path::new(range.path().as_ref())).to_string(),
            text: String::new(),
            path: range.path().to_string(),
            modified_side: modified,
            line: range.end(),
        }
    }

    /// Asks for a comment on the selected lines and adds it to the review.
    fn comment_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.selected_range(cx) else { return };
        let note = self.note_for(&range, cx);
        self.ask_comment(note, window, cx);
    }

    fn ask_comment(&mut self, note: crate::state::ReviewNote, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx).auto_grow(3, 12).placeholder("What should change here?")
        });
        let view = cx.weak_entity();
        let focused = Rc::new(std::cell::Cell::new(false));
        let title = note.location.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, view, focused) = (input.clone(), view.clone(), focused.clone());
            let note = note.clone();
            let shown = input.clone();
            let add = move |window: &mut Window, cx: &mut App| {
                let text = input.read(cx).value().trim().to_string();
                if text.is_empty() {
                    return;
                }
                let comment = crate::state::ReviewNote { text, ..note.clone() };
                let _ = view.update(cx, |c, cx| {
                    c.review.push(comment);
                    cx.notify();
                });
                window.close_dialog(cx);
            };
            dialog
                .title(format!("Comment on {title}"))
                .w(px(560.))
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let input = shown.clone();
                        window.defer(cx, move |window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
                    }
                    content.child(gpui_kit::component::input::Textarea::new(&shown))
                })
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("review-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("review-add").primary().label("Add comment").on_click(move |_, window, cx| add(window, cx))),
                )
        });
    }

    /// Every review comment to the agent in one message, pasted for the user
    /// to send; the review starts over.
    fn send_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.is_empty() {
            return;
        }
        let mut message = String::from("Review comments on the changes:\n");
        for (i, c) in self.review.iter().enumerate() {
            message.push_str(&format!(
                "\n{}. {}\n```{}\n{}\n```\n{}\n",
                i + 1,
                c.location,
                c.language,
                c.code.trim_end_matches('\n'),
                c.text
            ));
        }
        self.review.clear();
        cx.notify();
        let _ = self.task.update(cx, |t, cx| t.paste_to_agent(message, window, cx));
    }

    fn render_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.review.is_empty() {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        Some(
            v_flex()
                .px_3()
                .py_2()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary.opacity(0.5))
                .child(
                    h_flex()
                        .justify_between()
                        .child(div().text_xs().text_color(muted).child(format!("REVIEW ({})", self.review.len())))
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("review-clear")
                                        .xsmall()
                                        .ghost()
                                        .label("Clear")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.review.clear();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("review-send")
                                        .xsmall()
                                        .primary()
                                        .label("Send to agent")
                                        .on_click(cx.listener(|this, _, window, cx| this.send_review(window, cx))),
                                ),
                        ),
                )
                .children(self.review.iter().enumerate().map(|(i, c)| {
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(div().flex_none().font_family(cx.theme().mono_font_family.clone()).text_xs().text_color(muted).child(c.location.clone()))
                        .child(div().flex_1().min_w_0().truncate().child(c.text.clone()))
                        .child(
                            Button::new(("review-remove", i))
                                .xsmall()
                                .ghost()
                                .label("×")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if i < this.review.len() {
                                        this.review.remove(i);
                                    }
                                    cx.notify();
                                })),
                        )
                }))
                .into_any_element(),
        )
    }

    /// The lines selected by their numbers, to the agent with their location.
    fn send_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((location, code, language)) = self.selected(cx) else { return };
        let _ = self.task.update(cx, |t, cx| t.send_to_agent(location, code, language, window, cx));
    }

    fn set_scope(&mut self, scope: DiffScope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        // Read from the task rather than asking it to push: it would read this
        // view back while it is being updated.
        let Ok((files, lines, hunks)) = self.task.read_with(cx, |task, _| match scope {
            DiffScope::Uncommitted => (task.uncommitted_diff.clone(), task.uncommitted_lines.clone(), task.uncommitted_hunks.clone()),
            DiffScope::Branch => (task.branch_diff.clone(), task.branch_lines.clone(), task.branch_hunks.clone()),
        }) else {
            return;
        };
        self.first_lines = lines;
        self.hunks = hunks;
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
                        let (root, task, view, known) = (self.root.clone(), self.task.clone(), cx.weak_entity(), self.branches.clone());
                        Button::new("pick-base")
                            .small()
                            .ghost()
                            .label("▾")
                            .tooltip("Compare with another branch")
                            .dropdown_menu(move |mut menu, _, cx| {
                                // As listed last; asked again for the next time it opens.
                                load_branches(root.clone(), known.clone(), cx);
                                let branches = known.borrow().clone();
                                for branch in branches {
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
            .children(self.render_review(cx))
            .child(div().id("changes-diff").relative().flex_1().min_h_0()
                .child(crate::zoom::wheel(crate::zoom::Kind::Changes))
                .on_action(cx.listener(|this, _: &SendDiffToAgent, window, cx| this.send_selection(window, cx)))
                .on_action(cx.listener(|this, _: &CommentOnDiff, window, cx| this.comment_selection(window, cx)))
                .context_menu(|menu, _, _| {
                    menu.menu("Comment on selected lines…", Box::new(CommentOnDiff))
                        .menu("Send selected lines to Agent", Box::new(SendDiffToAgent))
                })
                .map(|this| {
                if empty {
                    this.flex()
                        .items_center()
                        .justify_center()
                        .text_color(cx.theme().muted_foreground)
                        .child("No uncommitted changes")
                } else {
                    let notes = Rc::new(self.review.clone());
                    let view = cx.weak_entity();
                    let add_view = cx.weak_entity();
                    // Under each change, what can be done with it alone.
                    let (hunks, task, staging) = (self.hunks.clone(), self.task.clone(), self.scope == DiffScope::Uncommitted);
                    let in_index = self.staged.clone();
                    let hunk_notes = hunks.iter().enumerate().map(|(i, h)| {
                        let side = if h.modified { DiffSide::Modified } else { DiffSide::Original };
                        DiffAnnotation::line(("hunk", i), DiffLinePosition::new(h.path.clone(), side, h.line as usize))
                    });
                    this.child(
                        Diff::new(&self.diff)
                            .annotations(
                                notes
                                    .iter()
                                    .enumerate()
                                    .map(|(i, n)| {
                                        let side = if n.modified_side { DiffSide::Modified } else { DiffSide::Original };
                                        DiffAnnotation::line(("review-note", i), DiffLinePosition::new(n.path.clone(), side, n.line))
                                    })
                                    .chain(hunk_notes)
                                    .collect::<Vec<_>>(),
                            )
                            .render_annotation(move |annotation, _, cx| {
                                let (kind, i) = match annotation.id() {
                                    ElementId::NamedInteger(kind, i) => (kind.clone(), *i as usize),
                                    _ => (SharedString::default(), usize::MAX),
                                };
                                if kind.as_ref() == "hunk" {
                                    let Some(hunk) = hunks.get(i).cloned() else { return div().into_any_element() };
                                    let (revert, stage, staged) = (task.clone(), task.clone(), hunk.clone());
                                    return h_flex()
                                        .justify_end()
                                        .gap_1()
                                        .px_2()
                                        .child(
                                            Button::new(("hunk-revert", i))
                                                .xsmall()
                                                .ghost()
                                                .label("↺ revert")
                                                .tooltip("Take this change back; the rest of the file stays (Undo follows)")
                                                .on_click(move |_, window, cx| {
                                                    let _ = revert.update(cx, |t, cx| t.revert_hunk(hunk.clone(), window, cx));
                                                }),
                                        )
                                        .when(staging, |row| {
                                            let is_staged = in_index.contains(&(staged.path.clone(), staged.changes.clone()));
                                            let (label, tip) = if is_staged { ("− unstage", "Take this change out of what is staged") } else { ("+ stage", "Stage this change alone") };
                                            row.child(Button::new(("hunk-stage", i)).xsmall().ghost().label(label).tooltip(tip).on_click(
                                                move |_, window, cx| {
                                                    let _ = stage.update(cx, |t, cx| t.stage_hunk(staged.clone(), !is_staged, window, cx));
                                                },
                                            ))
                                        })
                                        .into_any_element();
                                }
                                let text = notes.get(i).map(|n| n.text.clone()).unwrap_or_default();
                                let view = view.clone();
                                h_flex()
                                    .mx_2()
                                    .my_1()
                                    .px_3()
                                    .py_1p5()
                                    .gap_2()
                                    .rounded_md()
                                    .border_l_2()
                                    .border_color(cx.theme().primary)
                                    .bg(cx.theme().secondary)
                                    .text_sm()
                                    .child(div().flex_1().min_w_0().child(text))
                                    .child(Button::new(("review-note-remove", i)).xsmall().ghost().label("×").on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |c, cx| {
                                            if i < c.review.len() {
                                                c.review.remove(i);
                                            }
                                            cx.notify();
                                        });
                                    }))
                                    .into_any_element()
                            })
                            .on_add_annotation(move |range, window, cx| {
                                let _ = add_view.update(cx, |c, cx| {
                                    let note = c.note_for(range, cx);
                                    c.ask_comment(note, window, cx);
                                });
                            })
                            .code_font_size(crate::zoom::size(crate::zoom::Kind::Changes, cx))
                            .line_number(true)
                            .render_header_suffix({
                                let (root, task, lines) = (self.root.clone(), self.task.clone(), self.first_lines.clone());
                                move |file, _, cx| {
                                    h_flex()
                                        .gap_2()
                                        .when(staging, |row| row.child(viewed_checkbox(&task, file, cx)))
                                        .child(open_file_button(&root, &task, file, &lines))
                                }
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
    /// The task's services (`.ideae.toml`): start, stop, open.
    Run,
}

const NAV_LIMIT: usize = 100;
/// How far down the viewport a jump puts its line, unless already in view.
const JUMP_AT: f32 = 1. / 3.;

/// A place in the navigation history. Files go by path, so a closed one
/// opens again; other tabs by id, and are skipped once closed.
#[derive(Clone, Debug)]
enum NavPoint {
    File { path: PathBuf, line: u32, col: u32 },
    Tab(usize),
}

impl NavPoint {
    /// The same line counts as the same place: a step back should go somewhere.
    fn same_place(&self, other: &NavPoint) -> bool {
        match (self, other) {
            (NavPoint::File { path: a, line: la, .. }, NavPoint::File { path: b, line: lb, .. }) => a == b && la == lb,
            (NavPoint::Tab(a), NavPoint::Tab(b)) => a == b,
            _ => false,
        }
    }
}

struct TabItem {
    /// Stable across inserts and removals, unlike the index.
    id: usize,
    title: SharedString,
    kind: TabKind,
    /// In the right pane; every tab is in the left one when not split.
    right: bool,
    /// What the tab's label needs of its view, kept here: the task is not
    /// to be drawn again whenever a terminal or a page draws a frame, as it
    /// would be if drawing it read them. The terminal's or the page's
    /// title, whether the file has unsaved edits, the service it runs.
    shown: SharedString,
    dirty: bool,
    service: Option<String>,
}

impl TabItem {
    fn new(title: impl Into<SharedString>, kind: TabKind) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        Self { id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), title: title.into(), kind, right: false, shown: SharedString::default(), dirty: false, service: None }
    }
}

struct ChangedFile {
    path: SharedString,
    /// Of its part of the diff, to tell whether it changed since viewed.
    hash: u64,
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
    /// What the working agent does now: its latest tool call, in words.
    pub agent_activity: Option<String>,
    /// The file the agent is writing to now, and those it wrote to since
    /// the last prompt: marked in the Changes and Files panels.
    agent_editing: Option<SharedString>,
    agent_touched: Rc<HashSet<SharedString>>,
    /// The paths of `files`, for the tree's marks.
    changed_paths: Rc<HashSet<SharedString>>,
    /// The command that starts the agent, for Restart.
    agent: Option<String>,
    /// Text for the agent's prompt once it is ready to take it, and whether
    /// it is sent too.
    pending_prompt: Option<(String, bool)>,
    /// A message for an agent that was busy: sent when its turn ends.
    queued_message: Option<String>,
    /// Changed files you marked viewed, with their diff's hash then: a new
    /// change to one unmarks it.
    viewed: BTreeMap<String, u64>,
    /// Files you were in, the latest first, with where the cursor was.
    recent: Vec<(PathBuf, u32, u32)>,
    /// Its pull request, as last seen on GitHub.
    pub pr: Option<crate::pr::Pr>,
    /// Its share of the ports of `.ideae.toml`'s services, and those services.
    pub port_slot: u32,
    services: Vec<crate::services::Service>,
    /// Why `.ideae.toml` could not be read, and what the project seems to
    /// run, for the Run panel; read when it is shown.
    services_error: Option<String>,
    services_found: Vec<crate::services::Found>,
    /// Ports the task's processes listen on, while the Run panel shows.
    listening: Vec<crate::services::Listening>,
    _listening_poll: Option<Task<()>>,
    hook_socket: Option<String>,
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
    /// Parsed diffs of both scopes; the Changes tab shows one of them.
    uncommitted_diff: Vec<DiffFile>,
    branch_diff: Vec<DiffFile>,
    /// The first changed line of each file, per scope, for "open".
    uncommitted_lines: Rc<HashMap<String, u32>>,
    branch_lines: Rc<HashMap<String, u32>>,
    /// The single changes of each, to take back or stage one alone.
    uncommitted_hunks: Rc<Vec<git::Hunk>>,
    /// Files with conflict markers left in them, and whether committing
    /// them as they are was asked for a second time.
    conflict_marked: Vec<(String, u32)>,
    markers_accepted: bool,
    branch_hunks: Rc<Vec<git::Hunk>>,
    /// The Changes tab shows an older diff than the one read last.
    diff_stale: bool,
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
    /// The outline being made, by what it is of.
    structure_job: std::cell::RefCell<(Option<(usize, u64)>, Task<()>)>,
    tree: Entity<TreeState>,
    /// The file list the tree was built from, and the folders open in it.
    tree_files: Arc<Vec<String>>,
    /// The tree is of an older list: it is built when the Files panel shows.
    tree_stale: bool,
    /// The same list for definition lookups running off the UI thread.
    shared_files: Arc<std::sync::RwLock<Arc<Vec<String>>>>,
    expanded: HashSet<SharedString>,
    /// Folders folded in the Changes tree, as "staged:dir" / "changes:dir".
    folded_changes: HashSet<String>,
    /// Out of view long enough (or never shown): no language servers run;
    /// they start when the task comes into view.
    pub lsp_asleep: bool,
    file_states: HashMap<String, git::FileState>,
    commit_input: Entity<gpui_kit::component::input::TextareaState>,
    /// The git/gh action running, and the outcome of the last one.
    busy: Option<&'static str>,
    /// The file the last Discard threw away, for Undo.
    /// It is taken where the discard runs, off the UI thread.
    discard_undo: Option<Arc<std::sync::Mutex<Option<git::FileSnapshot>>>>,
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
    /// Since when no open file uses a server.
    lsp_unused: HashMap<(&'static str, PathBuf), std::time::Instant>,
    /// Where you were before each jump, for Back and Forward.
    nav_back: Vec<NavPoint>,
    nav_forward: Vec<NavPoint>,
    navigating: bool,
    visible: bool,
    _poll: Task<()>,
}

impl TaskView {
    pub fn new(
        root: PathBuf,
        branch: String,
        agent: Option<String>,
        hook_socket: Option<String>,
        port_slot: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let services = crate::services::load(&root).unwrap_or_default();
        let task_env = crate::services::task_env(&services, port_slot);
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
        match &agent {
            Some(agent) => tabs.push(Self::make_terminal(&root, hook_socket.as_ref(), "agent", 0, Some(agent.clone()), task_env.clone(), None, window, cx)),
            None => tabs.push(Self::make_terminal(&root, hook_socket.as_ref(), "shell", 1, None, task_env.clone(), None, window, cx)),
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
        // Out of view a task is looked at once a minute: every open task pays
        // for its refresh with a handful of git processes.
        let tick_tx = refresh_tx.clone();
        cx.spawn(async move |this, cx| {
            let mut n = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_secs(10)).await;
                n += 1;
                let Ok(visible) = this.read_with(cx, |t, _| t.visible) else { break };
                if (visible || n % 6 == 0) && tick_tx.send(()).await.is_err() {
                    break;
                }
            }
        })
        .detach();

        let poll_root = root.clone();
        let base_override: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let poll_base = base_override.clone();
        let poll = cx.spawn_in(window, async move |this, cx| {
            // The patches shown, by hash: only a new one is parsed.
            let mut seen = (text_hash(""), text_hash(""));
            let mut known = git::Known::default();
            loop {
                let root = poll_root.clone();
                let picked = poll_base.lock().unwrap().clone();
                let Ok((open, open_known)) = this.update(cx, |this, cx| this.open_files_known(cx)) else { break };
                let (read, patch, branch, staged, on_disk, now_seen, now_known) = cx
                    .background_spawn(async move {
                        let read = git::refresh(&root, picked, &mut known);
                        // Nothing committed on the branch yet: one patch, parsed once.
                        let same = read.branch == read.working;
                        let patch = parse_patch(read.working.clone(), &mut seen.0);
                        let branch = match &patch {
                            Some(patch) if same => {
                                seen.1 = seen.0;
                                Some(patch.clone())
                            }
                            _ => parse_patch(read.branch.clone(), &mut seen.1),
                        };
                        let staged: HashSet<(String, String)> = git::hunks(&read.staged).into_iter().map(|hunk| (hunk.path, hunk.changes)).collect();
                        (read, patch, branch, staged, changed_on_disk(open_known), seen, known)
                    })
                    .await;
                let git::Refreshed { files, watch, info, states, base, .. } = read;
                known = now_known;
                seen = now_seen;
                let applied = this.update_in(cx, |this, window, cx| {
                    for (file, change) in open.iter().zip(on_disk) {
                        if let Some(change) = change {
                            let _ = file.update(cx, |f, cx| f.apply_disk(change, window, cx));
                        }
                    }
                    if this.file_states != states {
                        this.file_states = states;
                        cx.notify();
                    }
                    if let Some(patch) = patch {
                        this.apply_patch(patch, cx);
                    }
                    if this.changes.read(cx).staged.as_ref() != &staged {
                        this.changes.update(cx, |changes, cx| {
                            changes.staged = Rc::new(staged);
                            cx.notify();
                        });
                    }
                    this.apply_branch_patch(base, branch, cx);
                    this.apply_files(files, cx);
                    this.apply_branch(info, cx);
                    this.sync_watches(watch);
                });
                if applied.is_err() {
                    break;
                }
                // Wait for a change, then let a burst of events settle. A
                // task out of view (its agent writing away) waits longer,
                // unless it comes into view meanwhile.
                if refresh_rx.recv().await.is_err() {
                    break;
                }
                for _ in 0..HIDDEN_REFRESH_WAITS {
                    cx.background_executor().timer(Duration::from_millis(150)).await;
                    if !matches!(this.read_with(cx, |t, _| t.visible), Ok(false)) {
                        break;
                    }
                }
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
            agent_activity: None,
            agent_editing: None,
            agent_touched: Rc::default(),
            changed_paths: Rc::default(),
            agent: agent.clone(),
            pending_prompt: None,
            queued_message: None,
            recent: Vec::new(),
            pr: None,
            viewed: BTreeMap::new(),
            port_slot,
            services,
            services_error: None,
            services_found: Vec::new(),
            listening: Vec::new(),
            _listening_poll: None,
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
            uncommitted_diff: Vec::new(),
            branch_diff: Vec::new(),
            uncommitted_lines: Rc::default(),
            branch_lines: Rc::default(),
            uncommitted_hunks: Rc::default(),
            conflict_marked: Vec::new(),
            markers_accepted: false,
            branch_hunks: Rc::default(),
            diff_stale: false,
            branch_info: git::BranchInfo::default(),
            right: RightPane::Changes,
            structure_file: None,
            side_focus: cx.focus_handle(),
            side_selected: 0,
            side_has_focus: Default::default(),
            structure: Default::default(),
            structure_job: std::cell::RefCell::new((None, Task::ready(()))),
            tree,
            tree_files: Arc::default(),
            tree_stale: false,
            shared_files: Arc::default(),
            expanded: HashSet::new(),
            folded_changes: HashSet::new(),
            lsp_asleep: false,
            file_states: HashMap::new(),
            commit_input,
            busy: None,
            discard_undo: None,
            action: None,
            refresh_tx,
            base_override,
            watcher,
            watched: Default::default(),
            lsp: HashMap::new(),
            lsp_unused: HashMap::new(),
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
            navigating: false,
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
        hook_socket: Option<&String>,
        title: &str,
        seq: usize,
        input: Option<String>,
        extra_env: Vec<(String, String)>,
        service: Option<(String, PathBuf)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabItem {
        let mut env: Vec<(String, String)> = extra_env;
        // Every terminal of the task can reach the hook socket, so a claude
        // started by hand reports too once given the settings file.
        if let Some(socket) = hook_socket {
            env.push((crate::agent::SOCK_ENV.into(), socket.clone()));
            env.push((crate::agent::TASK_ENV.into(), root.display().to_string()));
        }
        // Set either way: an ideae started from an agent's terminal inherits "agent".
        let role = if service.is_some() {
            "service"
        } else if input.is_some() {
            "agent"
        } else {
            "terminal"
        };
        let cwd = service.as_ref().map_or_else(|| root.clone(), |(_, cwd)| cwd.clone());
        env.push((crate::agent::ROLE_ENV.into(), role.into()));
        // Enter is a carriage return; PowerShell's line editor takes a bare
        // newline for a line break inside the command and waits for more.
        let enter = if cfg!(windows) { "\r" } else { "\n" };
        let input = input.map(|input| format!("{input}{enter}"));
        let config = TerminalConfig::default().with_colors(crate::theme::terminal_colors(cx));
        // In the daemon the program outlives the window; a restart attaches
        // to it again under the same session name.
        let session = format!(
            "{}#{}",
            root.display(),
            match (&service, role) {
                (Some((name, _)), _) => format!("service-{name}"),
                (None, "agent") => "agent".to_string(),
                _ => format!("shell-{seq}"),
            }
        );
        #[cfg(unix)]
        let daemon = crate::daemon::client::get();
        #[cfg(not(unix))]
        let daemon: Option<()> = None;
        #[cfg(not(unix))]
        let _ = &session;
        let (state, session) = match daemon {
            #[cfg(unix)]
            Some(daemon) => {
                let spec = crate::daemon::proto::Spec { cwd: cwd.clone(), env, input };
                let id = session.clone();
                (cx.new(|cx| daemon.terminal(id, spec, config, window, cx)), Some(session))
            }
            _ => {
                let mut options = LocalTerminalOptions::default().with_cwd(WorkingDirectory::Path(cwd.clone()));
                for (name, value) in env {
                    options = options.with_env(name, value);
                }
                if let Some(input) = input {
                    options = options.with_input(input);
                }
                (cx.new(|cx| TerminalState::local(options, config, window, cx)), None)
            }
        };
        cx.subscribe_in(&state, window, |this: &mut Self, state, event: &TerminalEvent, window, cx| match event {
            // A shell that exits (Ctrl+D, `exit`) takes its tab with it, as in a
            // terminal app; the agent's tab stays and shows the exit.
            TerminalEvent::Exited(_) => {
                let ix = this.tabs.iter().position(|t| match &t.kind {
                    TabKind::Terminal(v) => v.read(cx).state == *state,
                    _ => false,
                });
                match ix {
                    Some(ix) if this.tabs[ix].title != "agent" => this.remove_tab(ix, window, cx),
                    Some(_) => this.agent_status = crate::agent::AgentStatus::Exited,
                    None => {}
                }
                cx.emit(TaskEvent::Changed);
                cx.notify();
            }
            // Claude Code that crashed or was killed sends no SessionEnd.
            TerminalEvent::ForegroundChanged => {
                let ended = this.tabs.iter().any(|t| match &t.kind {
                    TabKind::Terminal(v) if t.title == "agent" && v.read(cx).state == *state => v.read(cx).agent_ended(cx),
                    _ => false,
                });
                if ended && this.agent_status != crate::agent::AgentStatus::Exited {
                    this.agent_status = crate::agent::AgentStatus::Exited;
                    cx.emit(TaskEvent::Changed);
                    cx.notify();
                }
            }
            TerminalEvent::TitleChanged | TerminalEvent::Bell => {
                let title = state.read(cx).title().clone();
                if let Some(tab) = this.tabs.iter_mut().find(|t| matches!(&t.kind, TabKind::Terminal(v) if v.read(cx).state == *state)) {
                    tab.shown = title;
                }
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
        let task = (role == "agent").then(|| cx.weak_entity());
        let service_name = service.map(|(name, _)| name);
        let shown = state.read(cx).title().clone();
        let view = cx.new(|cx| TermView::new(id, state, session, seq, task, service_name.clone(), cx));
        TabItem { shown, service: service_name, ..TabItem::new(title.to_string(), TabKind::Terminal(view)) }
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
                .on_action(cx.listener(|this, _: &ShowUsages, window, cx| this.show_usages_at_cursor(window, cx)))
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
        let text = format!("{location}\n```{language}\n{}\n```\n", code.trim_end_matches('\n'));
        self.paste_to_agent(text, window, cx);
    }

    /// Starts the agent again in its tab: typed into its shell, or in a new
    /// terminal when the shell is gone too.
    pub fn restart_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(command) = self.agent.clone() else { return };
        let Some(ix) = self.tabs.iter().position(|t| t.title == "agent" && matches!(t.kind, TabKind::Terminal(_))) else {
            return;
        };
        let TabKind::Terminal(view) = &self.tabs[ix].kind else { return };
        let view = view.clone();
        let state = view.read(cx).state.clone();
        if TermView::gone(state.read(cx)) {
            TermView::end(&view, cx);
            let env = crate::services::task_env(&self.services, self.port_slot);
            let tab = Self::make_terminal(&self.root, self.hook_socket.as_ref(), "agent", 0, Some(command), env, None, window, cx);
            (self.tabs[ix].kind, self.tabs[ix].shown) = (tab.kind, tab.shown);
        } else {
            let enter = if cfg!(windows) { "\r" } else { "\n" };
            view.update(cx, |v, _| v.ran = false);
            state.update(cx, |s, cx| s.send_text(format!("{command}{enter}"), cx));
        }
        self.agent_status = crate::agent::AgentStatus::Unknown;
        self.activate(ix, window, cx);
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// Puts `text` into the agent's prompt once the agent is up: Claude Code
    /// says so with SessionStart; another agent gets a few seconds after it
    /// starts. Pasted before, the shell would read it.
    ///
    /// With `send` it is sent as well, not left for the user to finish.
    pub fn queue_prompt(&mut self, text: String, send: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent.is_none() {
            return;
        }
        self.pending_prompt = Some((text, send));
        cx.spawn_in(window, async move |this, cx| {
            let started = std::time::Instant::now();
            let mut running_since: Option<std::time::Instant> = None;
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let ready = this.update(cx, |t, cx| {
                    if t.pending_prompt.is_none() {
                        return None;
                    }
                    let hooked = !matches!(t.agent_status, crate::agent::AgentStatus::Unknown);
                    let running = t.tabs.iter().any(|tab| match &tab.kind {
                        TabKind::Terminal(v) if tab.title == "agent" => {
                            v.read(cx).state.read(cx).foreground().is_some_and(|p| !p.is_shell())
                        }
                        _ => false,
                    });
                    Some((hooked, running))
                });
                let Ok(Some((hooked, running))) = ready else { return };
                if running {
                    running_since.get_or_insert_with(std::time::Instant::now);
                }
                let waited = running_since.map_or(Duration::ZERO, |at| at.elapsed());
                // Its input box takes a moment after the hook.
                // An agent that reports is waited for: until it says it
                // is up it may be asking something (to trust the folder),
                // and the prompt would go into that.
                let patient = this.read_with(cx, |t, _| t.agent_reports()).unwrap_or(false);
                if (hooked && waited > Duration::from_millis(500))
                    || (!patient && (waited > Duration::from_secs(4) || started.elapsed() > Duration::from_secs(30)))
                    || started.elapsed() > Duration::from_secs(1800)
                {
                    break;
                }
            }
            let _ = this.update_in(cx, |t, window, cx| {
                // Sent only to an agent that said it is up. One that did not
                // may be asking something first (to trust the folder), and
                // Enter would answer that.
                let up = !matches!(t.agent_status, crate::agent::AgentStatus::Unknown) || !t.agent_reports();
                match t.pending_prompt.take() {
                    Some((text, true)) if up => t.submit_to_agent(text, window, cx),
                    Some((text, _)) => t.paste_to_agent(text, window, cx),
                    None => {}
                }
            });
        })
        .detach();
    }

    /// Text into the agent's prompt, for the user to add to and send.
    pub fn paste_to_agent(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tabs.iter().position(|t| t.title == "agent" && matches!(t.kind, TabKind::Terminal(_))) else {
            return;
        };
        let TabKind::Terminal(view) = &self.tabs[ix].kind else { return };
        let state = view.read(cx).state.clone();
        // Bracketed paste: Claude Code shows it as pasted text, not a prompt sent.
        state.update(cx, |s, cx| s.paste_text(text, cx));
        self.activate(ix, window, cx);
    }

    /// Text into the agent's prompt, and sent: Enter a moment after the
    /// paste, which the agent would otherwise take for a line of it.
    pub fn submit_to_agent(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.paste_to_agent(text, window, cx);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let _ = this.update(cx, |t, cx| {
                let state = t.tabs.iter().find_map(|tab| match &tab.kind {
                    TabKind::Terminal(view) if tab.title == "agent" => Some(view.read(cx).state.clone()),
                    _ => None,
                });
                if let Some(state) = state {
                    state.update(cx, |s, cx| s.send_text("\r", cx));
                }
            });
        })
        .detach();
    }

    /// A message to the agent that does not land in the middle of its
    /// work or in a question it asks: a busy agent gets it when its turn
    /// ends, after what waits for that already. An agent that is not up
    /// gets nothing: typed there, it would run in a shell or answer a
    /// dialog.
    pub fn message_agent(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) -> Told {
        use crate::agent::AgentStatus::*;
        match self.agent_status {
            Working | NeedsInput(_) => {
                self.queued_message = Some(match self.queued_message.take() {
                    Some(waiting) => format!("{waiting}\n\n{text}"),
                    None => text,
                });
                Told::Later
            }
            Idle | Done => {
                self.submit_to_agent(text, window, cx);
                Told::Now
            }
            Unknown | Exited => Told::Not,
        }
    }

    /// Whether the agent is one that says when it is up: Claude Code,
    /// started with the hooks.
    fn agent_reports(&self) -> bool {
        self.agent.as_deref().is_some_and(|command| command.contains(" --settings "))
    }

    pub fn has_queued_message(&self) -> bool {
        self.queued_message.is_some()
    }

    /// The agent's turn ended: what waited for that goes out.
    pub fn deliver_queued(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.queued_message.take() {
            self.submit_to_agent(text, window, cx);
        }
    }

    /// Which of the views `show_view` knows is in front: the agent, the
    /// changes, the history or a shell.
    pub fn shown_view(&self) -> usize {
        match self.focused_ix().and_then(|ix| self.tabs.get(ix)) {
            Some(tab) => match &tab.kind {
                TabKind::Changes => 1,
                TabKind::History => 2,
                TabKind::Terminal(_) if tab.title != "agent" => 3,
                _ => 0,
            },
            None => 0,
        }
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

    /// Ctrl+Alt+B: the implementations and usages of the name at the cursor.
    fn show_usages_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(TabKind::File(file)) = self.focused_ix().map(|ix| &self.tabs[ix].kind) else { return };
        let (path, pos) = {
            let f = file.read(cx);
            let editor = f.editor.read(cx);
            (f.path.clone(), editor.text().offset_to_position(editor.cursor()))
        };
        self.show_places(path, pos, true, window, cx);
    }

    /// The usages of the name at `pos` in the open file `path`, after its
    /// implementations when `implementations`: from the language server, by
    /// text search without one. One place opens at once, several in a list.
    fn show_places(&mut self, path: PathBuf, pos: Position, implementations: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.tabs.iter().find_map(|t| match &t.kind {
            TabKind::File(f) if f.read(cx).path == path => Some(f.clone()),
            _ => None,
        }) else {
            return;
        };
        let (text, client) = {
            let f = file.read(cx);
            let client = f.lsp_client.clone().filter(|_| f.lsp_ok != Some(false));
            (f.editor.read(cx).text().clone(), client)
        };
        let Some(range) = text.word_range(text.position_to_offset(&pos)) else { return };
        let word = text.slice(range).to_string();
        if !word.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return;
        }
        let (root, files, buffer) = (self.root.clone(), self.shared_files.clone(), text.to_string());
        let query = PlacesQuery { path, pos, word: word.clone(), implementations };
        cx.spawn_in(window, async move |this, cx| {
            let places = cx
                .background_spawn(crate::procs::on_thread(move || {
                    let files = files.read().unwrap().clone();
                    find_places(&root, &files, &query, &buffer, client.as_deref())
                }))
                .await;
            let _ = this.update_in(cx, |this, window, cx| this.present_places(&word, implementations, places, window, cx));
        })
        .detach();
    }

    fn present_places(&mut self, word: &str, implementations: bool, places: Places, window: &mut Window, cx: &mut Context<Self>) {
        let what = if implementations { "Implementations and usages" } else { "Usages" };
        let mut all = places.implementations.iter().chain(&places.usages);
        match (all.next(), all.next()) {
            (None, _) => {
                let message = format!("{what} of {word}: none found");
                window.push_notification(gpui_kit::component::notification::Notification::info(message), cx);
            }
            (Some(only), None) => {
                let path = self.root.join(&only.path);
                self.open_file_at(path, only.line.saturating_sub(1), only.col, window, cx);
            }
            _ => {
                let by_search = |found: bool| if found { " · text search" } else { "" };
                let sections = vec![
                    (format!("Implementations{}", by_search(places.implementations_by_search)), places.implementations),
                    (format!("Usages{}", by_search(places.usages_by_search)), places.usages),
                ];
                let title = format!("{what} of {word}…");
                crate::finder::Finder::open_places(self.root.clone(), title, sections, cx.weak_entity(), window, cx);
            }
        }
    }

    /// "Install…" next to "kotlin-lsp not installed": asks first, as it
    /// downloads or builds; a copy another task installed is just used.
    fn confirm_lsp_install(&mut self, program: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::notification::Notification;
        if crate::lsp_install::installed(program).is_some() {
            return self.reattach_lsp(program, cx);
        }
        let Some(recipe) = crate::lsp_install::recipe(program) else { return };
        if let Some(tool) = crate::lsp_install::missing_tool(&recipe) {
            let message = format!("Installing {program} needs {tool} on PATH.");
            return window.push_notification(Notification::warning(message).title(format!("Install {program}")), cx);
        }
        let description = format!("{}\n\nInto {}.", recipe.summary, crate::lsp_install::root().join(program).display());
        let this = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            dialog
                .confirm()
                .title(format!("Install {program}?"))
                .description(description.clone())
                .ok_text("Install")
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.install_lsp(program, window, cx));
                    true
                })
        });
    }

    fn install_lsp(&mut self, program: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::notification::Notification;
        let install = cx.background_spawn(crate::procs::on_thread(move || { crate::lsp_install::install(program) }));
        cx.spawn_in(window, async move |this, cx| {
            // The headers say "installing" once the install has begun.
            cx.background_executor().timer(Duration::from_millis(50)).await;
            let _ = this.update(cx, |this, cx| this.notify_files(cx));
            let result = install.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => {
                        window.push_notification(Notification::success(format!("{program} installed")), cx);
                        this.reattach_lsp(program, cx);
                    }
                    Err(e) => window.push_notification(
                        Notification::error(format!("{e:#}")).title(format!("Installing {program} failed")),
                        cx,
                    ),
                }
                this.notify_files(cx);
            });
        })
        .detach();
    }

    fn notify_files(&self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let TabKind::File(file) = &tab.kind {
                file.update(cx, |_, cx| cx.notify());
            }
        }
    }

    /// Starts the newly installed server for the open files waiting for it.
    fn reattach_lsp(&mut self, program: &'static str, cx: &mut Context<Self>) {
        let waiting: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| match &tab.kind {
                TabKind::File(file) if file.read(cx).lsp_missing == Some(program) => Some(file.clone()),
                _ => None,
            })
            .collect();
        for file in waiting {
            let (path, editor) = {
                let f = file.read(cx);
                (f.path.clone(), f.editor.clone())
            };
            let content = editor.read(cx).text().to_string();
            self.attach_lsp(&path, content, &editor, &file, cx);
        }
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
        let files = self.tree_files.clone();
        if kind == crate::finder::FinderKind::Symbols {
            let symbols = symbol_source(self.root.clone(), self.lsp.values().cloned().collect(), files);
            return crate::finder::Finder::open_symbols(self.root.clone(), symbols, cx.weak_entity(), window, cx);
        }
        crate::finder::Finder::open(kind, self.root.clone(), files, cx.weak_entity(), window, cx);
    }

    /// A browser tab in the pane you are in. Playwright MCP can drive the
    /// same page through the endpoint shown in its toolbar.
    pub fn new_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_browser("about:blank", window, cx);
    }

    pub fn open_browser(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| crate::browser::BrowserView::new(url, window, cx));
        cx.subscribe_in(&view, window, |this, view, event: &crate::browser::BrowserEvent, window, cx| match event {
            crate::browser::BrowserEvent::Navigated => {
                let title = view.read(cx).title();
                if let Some(tab) = this.tabs.iter_mut().find(|t| matches!(&t.kind, TabKind::Browser(v) if v == view)) {
                    tab.shown = title;
                }
                cx.notify()
            }
            crate::browser::BrowserEvent::Picked(message) => this.paste_to_agent(message.clone(), window, cx),
        })
        .detach();
        let shown = view.read(cx).title();
        let mut tab = TabItem { shown, ..TabItem::new("web", TabKind::Browser(view)) };
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
            |rest| std::env::home_dir().unwrap_or_default().join(rest),
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
        let (root, files, symbol) = (self.root.clone(), self.shared_files.clone(), symbol.to_string());
        cx.spawn_in(window, async move |this, cx| {
            // It searches the project: not on the UI thread.
            let places = cx
                .background_spawn(async move {
                    let files = files.read().unwrap().clone();
                    find_symbol(&root, &files, &symbol)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match places.len() {
                0 => {}
                1 => {
                    let (path, line, _) = places.into_iter().next().unwrap();
                    this.open_file(path, line, window, cx);
                }
                _ => {
                    let hits = places
                        .into_iter()
                        .map(|(path, line, code)| {
                            let shown = path.strip_prefix(&this.root).unwrap_or(&path).display().to_string();
                            crate::finder::Hit { text: format!("{}   {shown}", code.trim()), path: shown, line: line + 1, col: 0, ranges: Vec::new() }
                        })
                        .collect();
                    crate::finder::Finder::open_hits(this.root.clone(), hits, cx.weak_entity(), window, cx)
                }
            });
        })
        .detach();
    }

    pub fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.new_terminal_tab(window, cx);
        self.tabs[at].right = self.focus_right && self.is_split();
        self.activate(at, window, cx);
    }

    /// Adds a shell tab after the last terminal and returns its index.
    fn new_terminal_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        self.terminal_seq += 1;
        self.terminal_tab(self.terminal_seq, window, cx)
    }

    /// A shell tab numbered `seq`: a restored one gets its old session back.
    fn terminal_tab(&mut self, seq: usize, window: &mut Window, cx: &mut Context<Self>) -> usize {
        self.terminal_seq = self.terminal_seq.max(seq);
        let env = crate::services::task_env(&self.services, self.port_slot);
        let tab = Self::make_terminal(&self.root, self.hook_socket.as_ref(), &format!("shell {seq}"), seq, None, env, None, window, cx);
        // After the last terminal, so terminals stay together left of the views.
        let at = self.tabs.iter().rposition(|t| matches!(t.kind, TabKind::Terminal(_))).map_or(0, |ix| ix + 1);
        self.tabs.insert(at, tab);
        at
    }

    /// The agent terminal's title, which Claude Code uses as a status line.
    pub fn agent_title(&self, _: &App) -> Option<SharedString> {
        self.tabs.iter().find_map(|tab| match &tab.kind {
            TabKind::Terminal(_) if tab.title == "agent" => Some(tab.shown.clone()),
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

    /// As `autosave_all`, and on disk when it returns: the task or the app
    /// goes next.
    pub fn save_all_now(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let TabKind::File(file) = &tab.kind {
                file.update(cx, |f, cx| f.save_now(cx));
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
        if status != crate::agent::AgentStatus::Working {
            self.agent_activity = None;
            self.agent_editing = None;
        }
        self.agent_status = status;
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// Keeps up with what the agent writes: `file` is what a tool call is
    /// about to change. A new prompt starts the turn's list over.
    pub fn follow_agent(&mut self, event: &str, file: Option<&str>) {
        match event {
            "UserPromptSubmit" => {
                self.agent_touched = Rc::default();
                self.agent_editing = None;
            }
            "PreToolUse" => {
                self.agent_editing = file.and_then(|file| path_in(&self.root, std::path::Path::new(file))).map(SharedString::from);
                Rc::make_mut(&mut self.agent_touched).extend(self.agent_editing.clone());
            }
            _ => {}
        }
    }

    /// "editing" by the file the agent writes to now, a dot by those it
    /// wrote to since the last prompt.
    fn agent_mark(&self, path: &str) -> Option<Div> {
        let editing = self.agent_editing.as_deref() == Some(path);
        (editing || self.agent_touched.contains(path))
            .then(|| div().text_xs().text_color(gpui_kit::blue()).child(if editing { "editing" } else { "●" }))
    }

    /// The open files look again at what they differ from: after the
    /// setting of what the gutter compares with changed.
    pub fn reload_gutters(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let TabKind::File(file) = &tab.kind {
                file.update(cx, |f, cx| f.load_blame(cx));
            }
        }
    }

    pub fn change_count(&self) -> usize {
        self.files.len()
    }

    /// Commits not pushed: ahead of the upstream, or the branch's own ones
    /// before it is published.
    pub fn unpushed(&self) -> u32 {
        self.branch_info.ahead
    }

    pub fn set_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = visible;
        self.sync_visibility(cx);
        if visible {
            // What it skipped while out of view.
            let _ = self.refresh_tx.try_send(());
            self.show_diff_if_seen(cx);
            self.fill_tree(cx);
            self.focus_active(window, cx);
            if self.right == RightPane::Run && self._listening_poll.is_none() {
                self.refresh_services(cx);
                self.watch_listening(cx);
            }
        }
    }

    /// Hidden terminals keep parsing but stop producing frames.
    fn sync_visibility(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            let shown = tab.id == self.left_active || Some(tab.id) == self.right_active;
            let visible = self.visible && shown;
            match &tab.kind {
                TabKind::Terminal(view) => view.read(cx).state.clone().update(cx, |s, cx| s.set_visible(visible, cx)),
                TabKind::Browser(view) => view.update(cx, |b, cx| b.set_visible(visible, cx)),
                _ => {}
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
        if ix >= self.tabs.len() {
            return;
        }
        if self.focused_ix() != Some(ix) {
            self.record_nav(cx);
            self.note_recent(cx);
        }
        let tab = &self.tabs[ix];
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
        self.show_diff_if_seen(cx);
        self.focus_active(window, cx);
        self.note_recent(cx);
        cx.notify();
    }

    /// The focused file goes to the front of the recent ones.
    fn note_recent(&mut self, cx: &App) {
        let Some(TabKind::File(file)) = self.focused_ix().map(|ix| &self.tabs[ix].kind) else { return };
        let f = file.read(cx);
        let entry = (f.path.clone(), f.cursor_line, f.cursor_col);
        self.recent.retain(|(p, _, _)| *p != entry.0);
        self.recent.insert(0, entry);
        self.recent.truncate(50);
    }

    /// Recent files for Ctrl+E, the one you are in left out.
    pub fn recent_files(&self, cx: &App) -> Vec<PathBuf> {
        let current = match self.focused_ix().map(|ix| &self.tabs[ix].kind) {
            Some(TabKind::File(f)) => Some(f.read(cx).path.clone()),
            _ => None,
        };
        let mut files: Vec<PathBuf> = self.recent.iter().map(|(p, _, _)| p.clone()).collect();
        // Open tabs restored from the last run count too, after the ones visited.
        for tab in &self.tabs {
            if let TabKind::File(f) = &tab.kind {
                let path = f.read(cx).path.clone();
                if !files.contains(&path) {
                    files.push(path);
                }
            }
        }
        files.retain(|p| Some(p) != current.as_ref() && p.exists());
        files
    }

    /// Back to a recent file: its tab as it is, or opened where the cursor was.
    pub fn reopen_recent(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::File(f) if f.read(cx).path == path));
        match open {
            Some(ix) => {
                self.record_nav(cx);
                self.activate(ix, window, cx);
            }
            None => {
                let (line, col) = self.recent.iter().find(|(p, _, _)| *p == path).map_or((0, 0), |(_, l, c)| (*l, *c));
                self.open_file_at(path, line, col, window, cx);
            }
        }
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
                    TermView::end(&view, cx);
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
                let view = view.clone();
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let task = task.clone();
                    let view = view.clone();
                    dialog
                        .confirm()
                        .title(format!("Close \"{title}\"?"))
                        .description(format!("It is still running {program}, which will be stopped."))
                        .ok_text("Close")
                        .on_ok(move |_, window, cx| {
                            TermView::end(&view, cx);
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
        // The server keeps every text it was given until told otherwise.
        if let TabKind::File(file) = &removed.kind {
            let f = file.read(cx);
            if let Some(client) = f.lsp_client.clone() {
                let path = f.path.clone();
                cx.background_spawn(crate::procs::on_thread(move || {
                    let _ = client.did_close(&path);
                }))
                .detach();
            }
        }
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

    fn apply_patch(&mut self, parsed: ParsedPatch, cx: &mut Context<Self>) {
        let ParsedPatch { files, first_lines, hashes, hunks, marked } = parsed;
        self.uncommitted_hunks = Rc::new(hunks);
        if self.conflict_marked != marked {
            self.conflict_marked = marked;
            self.markers_accepted = false;
        }
        self.files = files
            .iter()
            .map(|f| ChangedFile {
                path: f.path().clone(),
                hash: hashes.get(f.path().as_ref()).copied().unwrap_or(0),
                first_line: first_lines.get(f.path().as_ref()).copied().unwrap_or(1),
                status: f.status(),
                additions: f.additions(),
                deletions: f.deletions(),
            })
            .collect();
        // A file that is no longer changed is no longer viewed either.
        let changed: HashSet<SharedString> = self.files.iter().map(|f| f.path.clone()).collect();
        self.viewed.retain(|path, _| changed.contains(path.as_str()));
        self.changed_paths = Rc::new(changed);
        self.uncommitted_diff = files;
        self.uncommitted_lines = Rc::new(first_lines);
        self.refresh_scope(DiffScope::Uncommitted, cx);
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// The diff against `base`; `parsed` when it is another one than before.
    fn apply_branch_patch(&mut self, base: Option<String>, parsed: Option<ParsedPatch>, cx: &mut Context<Self>) {
        let base_changed = self.changes.read(cx).base != base;
        if parsed.is_none() && !base_changed {
            return;
        }
        if let Some(parsed) = parsed {
            self.branch_diff = parsed.files;
            self.branch_lines = Rc::new(parsed.first_lines);
            self.branch_hunks = Rc::new(parsed.hunks);
        }
        self.changes.update(cx, |c, _| c.base = base);
        self.refresh_scope(DiffScope::Branch, cx);
    }

    /// New data for `changed`: reload the diff only if that scope is on screen,
    /// so scrolling the other one is not reset.
    fn refresh_scope(&mut self, changed: DiffScope, cx: &mut Context<Self>) {
        if self.changes.read(cx).scope == changed {
            self.diff_stale = true;
            self.show_diff_if_seen(cx);
        }
        let counts = (self.uncommitted_diff.len(), self.branch_diff.len());
        self.changes.update(cx, |c, cx| {
            if c.counts != counts {
                c.counts = counts;
                cx.notify();
            }
        });
    }

    /// The Changes tab gets a new diff when it is looked at: laying one out
    /// and colouring it for a tab that is closed, behind another, or in a
    /// task out of view would be done again on every change an agent makes.
    fn show_diff_if_seen(&mut self, cx: &mut Context<Self>) {
        let seen = self.visible
            && self.tabs.iter().any(|t| matches!(t.kind, TabKind::Changes) && (t.id == self.left_active || Some(t.id) == self.right_active));
        if self.diff_stale && seen {
            self.diff_stale = false;
            self.show_scope(cx);
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
        let (files, lines) = match scope {
            DiffScope::Uncommitted => (self.uncommitted_diff.clone(), self.uncommitted_lines.clone()),
            DiffScope::Branch => (self.branch_diff.clone(), self.branch_lines.clone()),
        };
        let hunks = match scope {
            DiffScope::Uncommitted => self.uncommitted_hunks.clone(),
            DiffScope::Branch => self.branch_hunks.clone(),
        };
        let diff = self.changes.read(cx).diff.clone();
        diff.update(cx, |state, cx| state.set_files(files, cx));
        self.changes.update(cx, |c, cx| {
            c.counts = counts;
            c.first_lines = lines;
            c.hunks = hunks;
            cx.notify();
        });
    }

    /// The open files and the time each knows its file by, for
    /// `changed_on_disk`. One being saved is left out.
    fn open_files_known(&self, cx: &App) -> (Vec<WeakEntity<FileView>>, Vec<(PathBuf, Option<std::time::SystemTime>)>) {
        self.tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::File(f) => Some((f.downgrade(), f.read(cx).disk_known()?)),
                _ => None,
            })
            .unzip()
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
        if files == *self.tree_files {
            return;
        }
        // One list, shared by the tree, the finder and the lookups off the UI thread.
        let files = Arc::new(files);
        *self.shared_files.write().unwrap() = files.clone();
        // Class names for terminal links: `Profit` in output links to Profit.kt.
        gpui_cn::terminal::add_known_names(files.iter().filter_map(|f| {
            let name = f.rsplit('/').next()?;
            let stem = name.split('.').next()?;
            (stem.len() > 2 && stem.chars().next()?.is_ascii_uppercase()).then(|| stem.to_string())
        }));
        self.tree_files = files;
        self.tree_stale = true;
        self.fill_tree(cx);
    }

    /// The Files panel's tree, built when the panel is the one shown: it
    /// holds every path again, several times over, in every open task.
    fn fill_tree(&mut self, cx: &mut Context<Self>) {
        if self.tree_stale && self.visible && self.right == RightPane::Files {
            self.tree_stale = false;
            let items = build_tree(&self.tree_files, &self.expanded);
            self.tree.update(cx, |t, cx| t.set_items(items, cx));
        }
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
    /// Where you are: the file and cursor, or the tab that is no file.
    fn nav_point(&self, cx: &App) -> Option<NavPoint> {
        let tab = self.tabs.get(self.focused_ix()?)?;
        Some(match &tab.kind {
            TabKind::File(file) => {
                let f = file.read(cx);
                NavPoint::File { path: f.path.clone(), line: f.cursor_line, col: f.cursor_col }
            }
            _ => NavPoint::Tab(tab.id),
        })
    }

    /// Called before a jump: remembers the place it leaves.
    fn record_nav(&mut self, cx: &mut Context<Self>) {
        if self.navigating {
            return;
        }
        let Some(point) = self.nav_point(cx) else { return };
        if !self.nav_back.last().is_some_and(|last| last.same_place(&point)) {
            self.nav_back.push(point);
            if self.nav_back.len() > NAV_LIMIT {
                self.nav_back.remove(0);
            }
        }
        self.nav_forward.clear();
        cx.notify();
    }

    fn nav_reachable(&self, point: &NavPoint) -> bool {
        match point {
            NavPoint::File { path, .. } => path.is_file(),
            NavPoint::Tab(id) => self.ix_of(*id).is_some(),
        }
    }

    /// For the buttons, drawn every frame: without asking the disk whether
    /// the files are still there, as `navigate` does.
    pub fn can_navigate(&self, back: bool) -> bool {
        let stack = if back { &self.nav_back } else { &self.nav_forward };
        stack.iter().any(|p| match p {
            NavPoint::File { .. } => true,
            NavPoint::Tab(id) => self.ix_of(*id).is_some(),
        })
    }

    /// Mouse Back/Forward, Alt+Left/Right: the place before (or after) the
    /// last jump, a tab or a spot in a file.
    pub fn navigate(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let here = self.nav_point(cx);
        let target = loop {
            let stack = if back { &mut self.nav_back } else { &mut self.nav_forward };
            let Some(point) = stack.pop() else { return };
            if here.as_ref().is_some_and(|h| h.same_place(&point)) || !self.nav_reachable(&point) {
                continue;
            }
            break point;
        };
        if let Some(here) = here {
            let other = if back { &mut self.nav_forward } else { &mut self.nav_back };
            other.push(here);
        }
        self.navigating = true;
        match target {
            NavPoint::File { path, line, col } => self.open_file_at(path, line, col, window, cx),
            NavPoint::Tab(id) => {
                if let Some(ix) = self.ix_of(id) {
                    self.activate(ix, window, cx);
                }
            }
        }
        self.navigating = false;
        cx.notify();
    }

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
            2 => RightPane::Structure,
            _ => RightPane::Run,
        };
        self.fill_tree(cx);
        if self.right == RightPane::Run {
            self.refresh_services(cx);
            self.watch_listening(cx);
        }
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
            RightPane::Files | RightPane::Run => return,
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
                            file.update(cx, |f, cx| f.go_to_line(line, 0, window, cx));
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
        // While you type, the outline of a moment ago: `refresh_structure`
        // brings the new one once the typing pauses.
        if let Some((_, _, items)) = self.structure.borrow().as_ref().filter(|(i, _, _)| *i == id) {
            return items.clone();
        }
        let text = file.read(cx).editor.read(cx).text().to_string();
        let items = Rc::new(outline(&path, &text));
        *self.structure.borrow_mut() = Some((id, version, items.clone()));
        items
    }

    /// Parses the Structure panel's file again, off the UI thread, when its
    /// text is not the one the outline was made from.
    fn refresh_structure(&self, cx: &mut Context<Self>) {
        let Some((id, file)) = self.structure_target() else { return };
        let (path, version) = {
            let f = file.read(cx);
            (f.path.clone(), f.edit_version)
        };
        let current = Some((id, version));
        if self.structure.borrow().as_ref().map(|(i, v, _)| (*i, *v)) == current || self.structure_job.borrow().0 == current {
            return;
        }
        let job = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let (version, text) = file.read_with(cx, |f, cx| (f.edit_version, f.editor.read(cx).text().to_string()));
            let items = cx.background_spawn(async move { outline(&path, &text) }).await;
            let _ = this.update(cx, |this, cx| {
                *this.structure.borrow_mut() = Some((id, version, Rc::new(items)));
                cx.notify();
            });
        });
        *self.structure_job.borrow_mut() = (current, job);
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
            .child(
                h_flex()
                    .child(
                        Button::new("nav-back")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ArrowLeft)
                            .disabled(!self.can_navigate(true))
                            .tooltip("Back (Alt+Left, mouse Back)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.navigate(true, window, cx)
                            })),
                    )
                    .child(
                        Button::new("nav-forward")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ArrowRight)
                            .disabled(!self.can_navigate(false))
                            .tooltip("Forward (Alt+Right, mouse Forward)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.navigate(false, window, cx)
                            })),
                    ),
            )
            .child(div().font_semibold().child(format!("⎇ {}", if b.head.is_empty() { "…" } else { &b.head })))
            .when_some(b.upstream.clone(), |this, up| {
                this.child(div().text_color(cx.theme().muted_foreground).child(format!("→ {up}")))
                    .when(b.ahead > 0, |this| this.child(div().text_color(gpui_kit::green()).child(format!("↑{}", b.ahead))))
                    .when(b.behind > 0, |this| this.child(div().text_color(gpui_kit::red()).child(format!("↓{}", b.behind))))
            })
            .when(b.upstream.is_none() && !b.head.is_empty(), |this| {
                this.child(div().text_color(cx.theme().muted_foreground).child("no upstream"))
            })
            .when_some(b.base.clone().filter(|_| b.behind_base > 0), |this, base| {
                let busy = self.busy.is_some();
                this.child(
                    Button::new("rebase")
                        .xsmall()
                        .ghost()
                        .disabled(busy)
                        .label(format!("{base} ↓{} · Rebase", b.behind_base))
                        .tooltip(format!(
                            "Rebase this branch onto {base}. On a conflict nothing changes, and the agent's prompt gets a request to do it."
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.rebase_onto_base(base.clone(), window, cx)
                        })),
                )
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
            .children(self.render_pr_feedback(cx))
            .child(
                Button::new("log")
                    .xsmall()
                    .ghost()
                    .label("History")
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
                        .label(match (b.behind, b.pull_conflicts.is_empty()) {
                            (0, _) => "Pull".to_string(),
                            (behind, true) => format!("Pull ↓{behind}"),
                            (behind, false) => format!("Pull ↓{behind} ⚠"),
                        })
                        .when(!b.pull_conflicts.is_empty(), |button| button.text_color(gpui_kit::yellow()))
                        .tooltip(if b.pull_conflicts.is_empty() {
                            "Fast-forward, or rebase local commits; not tried when it would conflict".to_string()
                        } else {
                            format!("Would conflict with your work in {}: commit, or let the agent merge it first", b.pull_conflicts.join(", "))
                        })
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
        let changed = self.changed_paths.clone();
        let touched = self.agent_touched.clone();
        let editing = self.agent_editing.clone();
        div().flex_1().min_h_0().child(
            tree(&self.tree, move |ix, entry, selected, _, cx| {
                let item = entry.item();
                let label = if entry.is_folder() { StyledText::new(item.label.clone()) } else { file_label(item.label.clone(), cx) };
                let icon = if !entry.is_folder() {
                    IconName::File
                } else if entry.is_expanded() {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                };
                let is_changed = changed.contains(&item.id);
                // The agent's work shows in place of the plain "changed" dot;
                // a folder leads to the file being written.
                let on_the_way = editing.as_deref().is_some_and(|e| {
                    e.strip_prefix(item.id.as_ref()).is_some_and(|rest| rest.is_empty() || (item.is_folder() && rest.starts_with('/')))
                });
                let agent = if on_the_way && !item.is_folder() {
                    Some("editing")
                } else if on_the_way || touched.contains(&item.id) {
                    Some("●")
                } else {
                    None
                };
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
                            .child(div().truncate().child(label))
                            .when(is_changed && agent.is_none(), |this| this.child(div().text_xs().text_color(gpui_kit::green()).child("●")))
                            .when_some(agent, |this, mark| this.child(div().text_xs().text_color(gpui_kit::blue()).child(mark))),
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
        self.open_file_at(path, line, 0, window, cx)
    }

    /// Opens `path` (or its tab) with the cursor at 0-based `line`, `col`.
    pub fn open_file_at(&mut self, path: PathBuf, line: u32, col: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.record_nav(cx);
        if let Some(ix) = self.tabs.iter().position(|t| match &t.kind {
            TabKind::File(f) => f.read(cx).path == path,
            _ => false,
        }) {
            self.activate(ix, window, cx);
            if let TabKind::File(file) = &self.tabs[ix].kind {
                file.update(cx, |f, cx| f.go_to_line(line, col, window, cx));
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
            state.jump_to_position(Position::new(line, col), JUMP_AT, window, cx);
        });

        let saved: SharedString = content.clone().into();
        let root = self.root.clone();
        let task = cx.weak_entity();
        let file = cx.new(|cx| {
            // The editor notifies on cursor moves too: follow the line for blame.
            cx.observe(&editor, |this: &mut FileView, editor, cx| {
                let Position { line, character } = editor.read(cx).cursor_position();
                this.cursor_col = character;
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
                InputEvent::Blur => {
                    this.signature = None;
                    this.autosave(cx)
                }
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
            saving: None,
            save_seq: 0,
            write_gate: Arc::default(),
            _save: Task::ready(()),
            expand_stack: Vec::new(),
            last_expanded: None,
            root: root.clone(),
            task: task.clone(),
            blame: Rc::default(),
            cursor_line: line,
            cursor_col: col,
            _blame: Task::ready(()),
            lsp_status: SharedString::default(),
            lsp_ok: Some(false),
            lsp_client: None,
            lsp_program: String::new(),
            lsp_missing: None,
            preview: false,
            occurrence: None,
            committed: None,
            _marks: Task::ready(()),
            problems_seen: 0,
            signature: None,
            _signature: Task::ready(()),
            // Re-render while the server reports progress, so the header can
            // say "indexing" instead of a misleading "ready".
            _poll: cx.spawn_in(window, async move |this: WeakEntity<FileView>, cx| {
                let mut last = None;
                loop {
                    cx.background_executor().timer(Duration::from_millis(500)).await;
                    let Ok(busy) = this.update(cx, |f, cx| {
                        f.show_problems(cx);
                        f.lsp_client.as_ref().map(|c| (c.busy(), c.problem()))
                    }) else {
                        break;
                    };
                    if busy != last {
                        // Done with some work, indexing most of all: the server
                        // now knows names it could not tell a moment ago.
                        let learned = matches!((&last, &busy), (Some((Some(_), _)), Some((None, _))));
                        last = busy;
                        let told = this.update_in(cx, |f, window, cx| {
                            if learned {
                                f.editor.update(cx, |state, cx| state.refresh_semantic_tokens(window, cx));
                            }
                            cx.notify()
                        });
                        if told.is_err() {
                            break;
                        }
                    }
                }
            }),
        }});
        // The tab label shows whether the file has unsaved edits.
        // The cursor moving a line is the file's own to draw, except for the
        // Structure panel, which marks where it is.
        cx.observe(&file, |this, file, cx| {
            let dirty = file.read(cx).dirty;
            let Some(tab) = this.tabs.iter_mut().find(|t| matches!(&t.kind, TabKind::File(f) if *f == file)) else { return };
            let follows = this.right == RightPane::Structure && this.structure_file == Some(tab.id);
            if std::mem::replace(&mut tab.dirty, dirty) != dirty || follows {
                cx.notify();
            }
        })
        .detach();
        file.update(cx, |f, cx| f.load_blame(cx));
        self.attach_lsp(&path, content, &editor, &file, cx);
        self.use_search_definitions(&path, &editor, false, cx);

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
        let missing = lsp::missing_server(path);
        file.update(cx, |f, _| f.lsp_missing = missing);
        if self.lsp_asleep && lsp::server_for(path).is_ok() {
            return set_status(file, Some(false), ASLEEP.to_string(), cx);
        }
        let lsp::Server { language_id, program, command, args } = match lsp::server_for(path) {
            Ok(server) => server,
            Err(reason) => return set_status(file, Some(false), reason, cx),
        };
        let project = if path.starts_with(&self.root) {
            lsp::project_root(program, path, &self.root)
        } else {
            // A library file (std, the registry, a jump target): the task's own
            // server for the language reads it; one more would index it all again.
            self.lsp
                .keys()
                .find(|(p, _)| *p == program)
                .map(|(_, root)| root.clone())
                .unwrap_or_else(|| lsp::project_root(program, &self.root.join("_"), &self.root))
        };
        let key = (program, project.clone());
        let client = match self.lsp.get(&key) {
            Some(client) => client.clone(),
            None => match lsp::LspClient::spawn(program, &command, args, &project, crate::lsp_install::init_options(program, &command)) {
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
                .background_spawn(crate::procs::on_thread(move || { open_client.did_open(&open_path, language_id, &content) }))
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

        let bridge = Rc::new(LspBridge { client, uri: lsp::path_to_uri(path), path: path.clone(), file: file.downgrade() });
        let task = cx.weak_entity();
        let this_path = path.clone();
        editor.update(cx, |state, cx| {
            let lsp = state.lsp_mut();
            lsp.hover_provider = Some(bridge.clone());
            lsp.completion_provider = Some(bridge.clone());
            lsp.code_action_providers = vec![bridge.clone()];
            lsp.semantic_tokens_provider = Some(bridge.clone());
            lsp.definition_provider = Some(UsagesAtDeclaration::wrap(bridge, &this_path));
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
                if matches!(tab.kind, TabKind::Terminal(_)) && !tab.shown.is_empty() {
                    label = format!("{}: {}", tab.title, short_title(&tab.shown)).into();
                }
                if tab.title == "agent" {
                    if let Some((glyph, _)) = status_glyph(&self.agent_status) {
                        label = format!("{glyph} {label}").into();
                    }
                }
                if matches!(tab.kind, TabKind::Browser(_)) && !tab.shown.is_empty() && tab.shown != "about:blank" {
                    label = short_title(&tab.shown).into();
                }
                if matches!(tab.kind, TabKind::File(_)) {
                    if tab.dirty {
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
                let item = if tab.title == "agent" {
                    let root = self.root.clone();
                    Tab::new().child(
                        div()
                            .id("agent-usage")
                            .child(label)
                            .tooltip(move |_, cx| cx.new(|cx| crate::usage::UsageTooltip::new(root.clone(), cx)).into()),
                    )
                } else {
                    Tab::new().label(label)
                };
                let item = item
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
                                this.fill_tree(cx);
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
                    )
                    .child({
                        let running = self.running_services(cx).len();
                        Button::new("pane-run")
                            .small()
                            .ghost()
                            .label(if running > 0 { format!("Run ● {running}") } else { "Run".to_string() })
                            .tooltip(format!("Dev servers and workers of this task ({})", crate::keymap::label("run_panel")))
                            .selected(self.right == RightPane::Run)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.right = RightPane::Run;
                                this.refresh_services(cx);
                                this.watch_listening(cx);
                                cx.notify();
                            }))
                    }),
            )
            .when(self.right == RightPane::Run, |this| this.child(self.render_run_panel(cx)))
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
        self.refresh_structure(cx);
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
                        file.update(cx, |f, cx| f.go_to_line(line, 0, window, cx));
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
        let (push_label, push_tip) = if !b.has_origin {
            ("Push".to_string(), "No remote named origin")
        } else if b.upstream.is_none() {
            ("Publish".to_string(), "Push the branch to origin and track it")
        } else if b.ahead > 0 {
            (format!("Push ↑{}", b.ahead), "Push the commits to the upstream")
        } else {
            ("Push".to_string(), "Nothing to push")
        };
        let can_push = b.has_origin && (b.upstream.is_none() || b.ahead > 0);
        // Status above the message box: the box is pinned to the bottom, so
        // nothing below it moves when a status shows up.
        v_flex()
            .flex_none()
            .gap_1p5()
            .p_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .children(self.conflict_marked.iter().map(|(path, line)| {
                let (open, line) = (self.root.join(path), line.saturating_sub(1));
                div()
                    .id(SharedString::from(format!("marked-{path}")))
                    .text_xs()
                    .text_color(gpui_kit::red())
                    .cursor_pointer()
                    .child(format!("⚠ conflict left in {path}:{}", line + 1))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_file(open.clone(), line, window, cx)))
            }))
            .when_some(self.busy, |this, label| {
                this.child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!("{label}…")))
            })
            .when_some(self.action.clone().filter(|_| !busy), |this, (ok, message)| {
                this.child(
                    h_flex()
                        .gap_1()
                        .items_start()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(if ok { gpui_kit::green() } else { gpui_kit::red() })
                                .child(message.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().to_string()),
                        )
                        .when(self.discard_undo.is_some(), |row| {
                            row.child(
                                Button::new("undo-discard")
                                    .xsmall()
                                    .outline()
                                    .label("Undo")
                                    .on_click(cx.listener(|this, _, window, cx| this.undo_discard(window, cx))),
                            )
                        })
                        .child(
                            Button::new("dismiss-action")
                                .xsmall()
                                .ghost()
                                .label("×")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.action = None;
                                    this.discard_undo = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(gpui_kit::component::input::Textarea::new(&self.commit_input).small()))
                    .child(
                        Button::new("suggest-message")
                            .small()
                            .ghost()
                            .label("Write")
                            .disabled(busy || self.files.is_empty())
                            .tooltip(if staged > 0 {
                                "Claude writes the message for the staged changes"
                            } else {
                                "Claude writes the message for all the changes"
                            })
                            .on_click(cx.listener(|this, _, window, cx| this.suggest_message(window, cx))),
                    ),
            )
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
                            .disabled(busy || !can_push)
                            .label(push_label)
                            .tooltip(push_tip)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let upstream = this.branch_info.upstream.is_some();
                                this.run_action("Push", move |root| git::push(&root, upstream), false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("pr")
                            .small()
                            .ghost()
                            .disabled(busy || !b.has_origin)
                            .label("PR")
                            .tooltip(if b.has_origin {
                                "Open this branch's pull request, creating it if needed"
                            } else {
                                "No remote named origin"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                let upstream = this.branch_info.upstream.is_some();
                                this.run_action("PR", move |root| git::pull_request(&root, upstream), true, window, cx)
                            })),
                    ),
            )
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
            list = self.render_change_tree(list, &staged, true, cx);
        }
        if self.files.is_empty() {
            list = list.child(div().p_3().text_sm().text_color(cx.theme().muted_foreground).child("No uncommitted changes"));
        } else if !unstaged.is_empty() {
            let viewed = unstaged.iter().filter(|&&ix| self.is_viewed(ix)).count();
            let title = if viewed > 0 {
                format!("CHANGES ({}) · {viewed} viewed", unstaged.len())
            } else {
                format!("CHANGES ({})", unstaged.len())
            };
            list = list.child(section("stage-all", title, "stage all", true, cx));
            list = self.render_change_tree(list, &unstaged, false, cx);
        }
        list.overflow_y_scrollbar()
    }

    /// The files as a folder tree; a folded folder hides what is under it.
    fn render_change_tree(&self, mut list: Stateful<Div>, files: &[usize], in_staged: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let section = if in_staged { "staged" } else { "changes" };
        let paths: Vec<(usize, &str)> = files.iter().map(|&ix| (ix, self.files[ix].path.as_ref())).collect();
        let mut hidden_under: Option<String> = None;
        for node in change_tree(&paths) {
            let path = match &node {
                ChangeNode::Dir { path, .. } => path.clone(),
                ChangeNode::File { ix, .. } => self.files[*ix].path.to_string(),
            };
            if let Some(prefix) = &hidden_under {
                if path.starts_with(prefix.as_str()) {
                    continue;
                }
                hidden_under = None;
            }
            match node {
                ChangeNode::File { ix, depth } => list = list.child(self.render_change_row(ix, depth, in_staged, cx)),
                ChangeNode::Dir { path, label, depth } => {
                    let key = format!("{section}:{path}");
                    let folded = self.folded_changes.contains(&key);
                    if folded {
                        hidden_under = Some(format!("{path}/"));
                    }
                    let under = format!("{path}/");
                    let group = SharedString::from(format!("change-dir-{key}"));
                    list = list.child(
                        h_flex()
                            .id(SharedString::from(format!("dir-{key}")))
                            .group(group.clone())
                            .pl(px(12. + 14. * depth as f32))
                            .pr_3()
                            .py_0p5()
                            .gap_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().accent))
                            .child(div().w(px(12.)).text_xs().child(if folded { "▸" } else { "▾" }))
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            .child(
                                div().opacity(0.).group_hover(group, |s| s.opacity(1.)).child(
                                    Button::new(SharedString::from(format!("dir-stage-{key}")))
                                        .xsmall()
                                        .ghost()
                                        .label(if in_staged { "−" } else { "+" })
                                        .tooltip(if in_staged { "Unstage the folder" } else { "Stage the folder" })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            // Looked up on the click, not for every folder on every frame.
                                            let dir_files = this
                                                .files
                                                .iter()
                                                .filter(|f| f.path.starts_with(under.as_str()) && {
                                                    let state = this.state_of(&f.path);
                                                    if in_staged { state.staged } else { state.unstaged }
                                                })
                                                .map(|f| f.path.to_string())
                                                .collect();
                                            this.stage_paths(dir_files, !in_staged, window, cx)
                                        })),
                                ),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.folded_changes.remove(&key) {
                                    this.folded_changes.insert(key.clone());
                                }
                                cx.notify();
                            })),
                    );
                }
            }
        }
        list
    }

    fn is_viewed(&self, ix: usize) -> bool {
        let file = &self.files[ix];
        self.viewed.get(file.path.as_ref()) == Some(&file.hash)
    }

    fn is_path_viewed(&self, path: &str) -> bool {
        self.files.iter().any(|f| f.path.as_ref() == path && self.viewed.get(path) == Some(&f.hash))
    }

    fn toggle_viewed(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(file) = self.files.iter().find(|f| f.path.as_ref() == path) else { return };
        if self.viewed.get(path) == Some(&file.hash) {
            self.viewed.remove(path);
        } else {
            self.viewed.insert(path.to_string(), file.hash);
        }
        self.changes.update(cx, |_, cx| cx.notify());
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    fn render_change_row(&self, ix: usize, depth: usize, in_staged: bool, cx: &mut Context<Self>) -> AnyElement {
        let file = &self.files[ix];
        let path = file.path.clone();
        let stage_path = file.path.to_string();
        let open_path = self.root.join(file.path.as_ref());
        let open_line = file.first_line.saturating_sub(1);
        let deleted = matches!(file.status, DiffFileStatus::Deleted);
        // The tree shows the folders.
        let name = file.path.rsplit_once('/').map_or(file.path.to_string(), |(_, name)| name.to_string());
        let status = match file.status {
            DiffFileStatus::Added => "A",
            DiffFileStatus::Deleted => "D",
            DiffFileStatus::Renamed => "R",
            _ => "M",
        };
        let id = if in_staged { ix * 2 + 1 } else { ix * 2 };
        let keyboard = self.side_has_focus.get() && self.right == RightPane::Changes && ix == self.side_selected;
        let viewed = self.is_viewed(ix);
        let view_path = file.path.to_string();
        let discard_path = file.path.to_string();
        let view = cx.weak_entity();
        h_flex()
            .id(("file", id))
            .when(keyboard, |row| row.border_l_2().border_color(cx.theme().primary))
            .group("change-row")
            .pl(px(12. + 14. * depth as f32))
            .pr_3()
            .py_0p5()
            .gap_2()
            .text_sm()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().accent))
            .child(div().w(px(12.)).text_color(cx.theme().muted_foreground).child(status))
            .child(div().flex_1().min_w_0().truncate().when(viewed, |d| d.text_color(cx.theme().muted_foreground)).child(file_label(name.into(), cx)))
            .children(self.agent_mark(&file.path))
            .when(viewed, |row| row.child(div().text_xs().text_color(gpui_kit::green()).child("✓")))
            .child(div().text_xs().text_color(gpui_kit::green()).child(format!("+{}", file.additions)))
            .child(div().text_xs().text_color(gpui_kit::red()).child(format!("-{}", file.deletions)))
            // The actions show on the row under the pointer or the keyboard.
            .child(
                h_flex()
                    .gap_0p5()
                    .when(!keyboard, |actions| actions.invisible().group_hover("change-row", |s| s.visible()))
                    .child(
                        Button::new(("viewed", id))
                            .xsmall()
                            .ghost()
                            .label(if viewed { "✓" } else { "○" })
                            .tooltip(if viewed { "Viewed; click to unmark" } else { "Mark as viewed until it changes again" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_viewed(&view_path, cx)
                            })),
                    )
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
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.show_changed_file(path.clone(), window, cx)))
            .context_menu(move |menu, _, _| {
                use gpui_kit::component::menu::PopupMenuItem;
                let (open_view, open_path) = (view.clone(), open_path.clone());
                let (discard_view, discard_path) = (view.clone(), discard_path.clone());
                menu.when(!deleted, |menu| {
                    menu.item(PopupMenuItem::new("Open").on_click(move |_, window, cx| {
                        let path = open_path.clone();
                        open_view.update(cx, |this, cx| this.open_file(path, open_line, window, cx)).ok();
                    }))
                })
                .when(!in_staged, |menu| {
                    menu.item(PopupMenuItem::new("Discard changes").on_click(move |_, window, cx| {
                        let path = discard_path.clone();
                        discard_view.update(cx, |this, cx| this.confirm_discard(path, window, cx)).ok();
                    }))
                })
            })
            .into_any_element()
    }

    /// Takes one change of a file back; what the file was is kept for Undo
    /// until the next git action, as when the whole file is discarded.
    fn revert_hunk(&mut self, hunk: git::Hunk, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let kept: Arc<std::sync::Mutex<Option<git::FileSnapshot>>> = Arc::default();
        let slot = kept.clone();
        self.run_action(
            "Revert",
            move |root| {
                *slot.lock().unwrap() = Some(git::snapshot(&root, &hunk.path));
                git::apply_hunk(&root, &hunk.patch, true, false).map(|_| format!("Took a change of {} back", hunk.path))
            },
            false,
            window,
            cx,
        );
        self.discard_undo = Some(kept);
    }

    fn stage_hunk(&mut self, hunk: git::Hunk, stage: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.run_action(
            if stage { "Stage" } else { "Unstage" },
            move |root| {
                let done = if stage { "Staged" } else { "Unstaged" };
                git::stage_change(&root, &hunk, stage).map(|_| format!("{done} a change of {}", hunk.path))
            },
            false,
            window,
            cx,
        );
    }

    /// Discards at once; what was there is kept for Undo until the next
    /// git action.
    fn confirm_discard(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let kept: Arc<std::sync::Mutex<Option<git::FileSnapshot>>> = Arc::default();
        let slot = kept.clone();
        self.run_action(
            "Discard",
            move |root| {
                *slot.lock().unwrap() = Some(git::snapshot(&root, &path));
                git::discard(&root, &path).map(|_| format!("Discarded {path}"))
            },
            false,
            window,
            cx,
        );
        self.discard_undo = Some(kept);
    }

    fn undo_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.discard_undo.take().and_then(|kept| kept.lock().unwrap().take()) else { return };
        self.run_action("Undo", move |root| git::restore(&root, &snapshot), false, window, cx);
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

    /// Write: Claude writes the message for what would be committed.
    fn suggest_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Writing the message");
        self.action = None;
        cx.notify();
        let root = self.root.clone();
        let staged_only = !self.staged_paths().is_empty();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(crate::procs::on_thread(move || { git::suggest_message(&root, staged_only) })).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(message) => this.commit_input.update(cx, |i, cx| i.set_value(message, window, cx)),
                    Err(e) => this.action = Some((false, format!("Message: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.commit_input.read(cx).value().trim().to_string();
        if message.is_empty() {
            self.action = Some((false, "Write a commit message first".into()));
            cx.notify();
            return;
        }
        if !self.conflict_marked.is_empty() && !std::mem::replace(&mut self.markers_accepted, true) {
            self.action = Some((false, "Conflict markers are left in the files above. Commit again to commit them as they are.".into()));
            cx.notify();
            return;
        }
        let all = self.staged_paths().is_empty();
        self.run_action("Commit", move |root| git::commit(&root, &message, all), false, window, cx);
    }

    /// Runs a git/gh job off the UI thread, shows its outcome under the
    /// buttons, and refreshes. A successful commit clears the message.
    /// Rebase onto the base; conflicts go to the agent as a request.
    fn rebase_onto_base(&mut self, base: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Rebase");
        self.action = None;
        cx.notify();
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let job_base = base.clone();
            let result = cx.background_spawn(crate::procs::on_thread(move || { git::rebase_onto(&root, &job_base) })).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                this.action = Some(match result {
                    Ok(out) => (true, out),
                    Err(git::RebaseError::Failed(e)) => (false, format!("Rebase: {e}")),
                    Err(git::RebaseError::Conflicts(files)) => {
                        let list = files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n");
                        this.paste_to_agent(
                            format!(
                                "Rebase this branch onto {base} (`git fetch` first). It conflicts in:\n{list}\n\nResolve the conflicts keeping what both sides meant, check that it builds and the tests pass, then finish the rebase."
                            ),
                            window,
                            cx,
                        );
                        (false, format!("Rebase: {} conflicting file(s), nothing changed; the agent's prompt has a request", files.len()))
                    }
                });
                let _ = this.refresh_tx.try_send(());
                cx.notify();
            });
        })
        .detach();
    }

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
        self.discard_undo = None;
        cx.notify();
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(crate::procs::on_thread(move || { job(root) })).await;
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
            // Capture phase: terminals and editors handle mouse downs themselves.
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                if let MouseButton::Navigate(direction) = event.button {
                    cx.stop_propagation();
                    this.navigate(direction == NavigationDirection::Back, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleZoom, window, cx| this.toggle_zoom_focused(window, cx)))
            .on_action(cx.listener(|this, _: &ShowOutline, window, cx| this.show_outline(window, cx)))
            .on_action(cx.listener(|this, _: &ShowUsages, window, cx| this.show_usages_at_cursor(window, cx)))
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
                            .fixed()
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

/// A `git diff` as the panels need it, made off the UI thread.
#[derive(Clone)]
struct ParsedPatch {
    files: Vec<DiffFile>,
    first_lines: HashMap<String, u32>,
    hashes: HashMap<String, u64>,
    hunks: Vec<git::Hunk>,
    /// Files with a conflict left in them, and its line.
    marked: Vec<(String, u32)>,
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Parses `patch` unless it is the one seen last (`seen`, its hash).
fn parse_patch(patch: String, seen: &mut u64) -> Option<ParsedPatch> {
    let hash = text_hash(&patch);
    if std::mem::replace(seen, hash) == hash {
        return None;
    }
    let files = DiffFile::parse(&patch).inspect_err(|error| eprintln!("diff parse: {error}")).ok()?;
    Some(ParsedPatch {
        first_lines: git::first_changed_lines(&patch),
        hashes: file_patch_hashes(&patch),
        hunks: git::hunks(&patch),
        marked: git::conflict_marked(&patch),
        files: with_languages(files),
    })
}

/// Each file's part of a `git diff`, hashed, by its path after the change.
fn file_patch_hashes(patch: &str) -> HashMap<String, u64> {
    use std::hash::{Hash as _, Hasher as _};
    let mut out = HashMap::new();
    let mut current: Option<(String, std::collections::hash_map::DefaultHasher)> = None;
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            if let Some((path, hasher)) = current.take() {
                out.insert(path, hasher.finish());
            }
            // "a/x b/x": the path after " b/".
            let path = header.rsplit_once(" b/").map_or(header, |(_, b)| b).to_string();
            current = Some((path, std::collections::hash_map::DefaultHasher::new()));
        } else if let Some((_, hasher)) = current.as_mut() {
            line.hash(hasher);
        }
    }
    if let Some((path, hasher)) = current {
        out.insert(path, hasher.finish());
    }
    out
}

/// A glyph and its color for the agent status, if it has one to show.
/// `http://localhost:3100` of `http://localhost:3100/users?page=2`.
fn origin(url: &str) -> &str {
    let after_scheme = url.find("://").map_or(0, |at| at + 3);
    let end = url[after_scheme..].find(['/', '?', '#']).map_or(url.len(), |at| after_scheme + at);
    &url[..end]
}

/// What became of a message to an agent.
#[derive(Clone, Copy, PartialEq)]
pub enum Told {
    Now,
    /// When its turn ends.
    Later,
    /// It is not up.
    Not,
}

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

/// Declarations by a part of their name: from the task's language servers,
/// which know the project; from the grammars' reading of its files where
/// no server answers.
fn symbol_source(root: PathBuf, clients: Vec<Arc<lsp::LspClient>>, files: Arc<Vec<String>>) -> crate::finder::SymbolSource {
    use crate::finder::{DETAIL, Hit};
    const LIMIT: usize = 200;
    let declared: Arc<std::sync::OnceLock<Vec<Hit>>> = Arc::default();
    Arc::new(move |query: &str| {
        let mut hits = Vec::new();
        for client in &clients {
            for symbol in client.workspace_symbols(query).unwrap_or_default() {
                let kind = lsp::symbol_kind(symbol.kind);
                let detail = if symbol.container.is_empty() { kind.to_string() } else { format!("{kind} in {}", symbol.container) };
                hits.push(Hit {
                    path: symbol.path.strip_prefix(&root).unwrap_or(&symbol.path).to_string_lossy().into_owned(),
                    line: symbol.line + 1,
                    col: symbol.col,
                    text: format!("{}{DETAIL}{}", symbol.name, detail.trim()),
                    ranges: Vec::new(),
                });
            }
        }
        // The project's own before what its libraries declare.
        hits.sort_by_key(|hit| std::path::Path::new(&hit.path).is_absolute());
        if hits.is_empty() {
            let all = declared.get_or_init(|| {
                let mut all = Vec::new();
                for file in files.iter().take(5000) {
                    let path = root.join(file);
                    if std::fs::metadata(&path).map_or(true, |m| m.len() > 300_000) {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(&path) else { continue };
                    all.extend(outline(&path, &text).into_iter().filter_map(|item| {
                        let (kind, name) = item.label.rsplit_once(' ')?;
                        Some(Hit { path: file.clone(), line: item.line + 1, col: 0, text: format!("{name}{DETAIL}{kind}"), ranges: Vec::new() })
                    }));
                }
                all
            });
            hits = crate::finder::match_symbols(all, query, LIMIT);
        }
        hits.truncate(LIMIT);
        hits
    })
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
mod places_tests {
    use super::{Position, find_places, is_at, search_places, PlacesQuery};

    fn project(files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("ideae-places-{}-{}", std::process::id(), files.len()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        root
    }

    #[test]
    fn search_finds_implementations_and_usages_without_declarations() {
        let root = project(&[
            ("Shape.kt", "interface Shape {\n    fun area(): Double\n}\n"),
            ("Circle.kt", "class Circle(val r: Double) : Shape {\n    override fun area() = 3.14 * r * r\n}\n"),
            ("Square.java", "class Square implements Shape {}\n"),
            ("Main.kt", "fun total(shapes: List<Shape>) = shapes.sumOf { it.area() }\nval s: Shape = Circle(1.0)\n"),
        ]);
        let files = vec!["Shape.kt".into(), "Circle.kt".into(), "Square.java".into(), "Main.kt".into()];
        let (implementations, usages) = search_places(&root, &files, "Shape", true);
        let at = |hits: &[crate::finder::Hit]| hits.iter().map(|h| format!("{}:{}", h.path, h.line)).collect::<Vec<_>>();
        let mut found = at(&implementations);
        found.sort();
        assert_eq!(found, ["Circle.kt:1", "Square.java:1"]);
        let mut found = at(&usages);
        found.sort();
        assert_eq!(found, ["Main.kt:1", "Main.kt:2"]);
        assert_eq!(usages.iter().find(|h| h.line == 2).unwrap().col, 7);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_search_result_is_here_on_its_line() {
        let range = |a: (u32, u32), b: (u32, u32)| lsp_types::Range { start: Position::new(a.0, a.1), end: Position::new(b.0, b.1) };
        assert!(is_at(range((3, 0), (3, 0)), Position::new(3, 12)));
        assert!(!is_at(range((3, 0), (3, 0)), Position::new(4, 0)));
        assert!(is_at(range((3, 4), (3, 9)), Position::new(3, 9)));
        assert!(!is_at(range((3, 4), (3, 9)), Position::new(3, 10)));
    }

    /// Needs gopls: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn gopls_implementations_and_usages() {
        let root = project(&[
            ("go.mod", "module example.com/m\n\ngo 1.21\n"),
            ("shape.go", "package main\n\ntype Shape interface{ Area() float64 }\n\ntype Sq struct{}\n\nfunc (Sq) Area() float64 { return 1 }\n\nfunc main() { var s Shape = Sq{}; _ = s.Area() }\n"),
        ]);
        let file = root.join("shape.go");
        let text = std::fs::read_to_string(&file).unwrap();
        let client = crate::lsp::LspClient::spawn("gopls", std::path::Path::new("gopls"), &[], &root, serde_json::Value::Null).unwrap();
        client.did_open(&file, "go", &text).unwrap();
        let query = PlacesQuery { path: file.clone(), pos: Position::new(2, 6), word: "Shape".into(), implementations: true };
        // Ctrl+B on the declaration turns into its usages only if the server
        // answers with the declaration itself.
        let links = client.definition(&crate::lsp::path_to_uri(&file), query.pos).unwrap();
        assert!(!links.is_empty() && links.iter().all(|l| is_at(l.target_selection_range, query.pos)), "{links:?}");
        let places = find_places(&root, &[], &query, &text, Some(&client));
        assert!(!places.implementations_by_search && !places.usages_by_search);
        assert_eq!(places.implementations.iter().map(|h| h.line).collect::<Vec<_>>(), [5]);
        assert_eq!(places.usages.iter().map(|h| (h.line, h.text.as_str())).collect::<Vec<_>>(), [(9, "func main() { var s Shape = Sq{}; _ = s.Area() }")]);
        let _ = std::fs::remove_dir_all(&root);
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

/// A file name whose extension steps back: the name is what is read.
fn file_label(name: SharedString, cx: &App) -> StyledText {
    let extension = name
        .rfind('.')
        .filter(|&dot| dot > 0)
        .map(|dot| (dot..name.len(), HighlightStyle { color: Some(cx.theme().muted_foreground), ..Default::default() }));
    StyledText::new(name).with_highlights(extension)
}

/// "Viewed" in a diff file's header, the mark the Changes list shows too.
fn viewed_checkbox(task: &WeakEntity<TaskView>, file: &DiffFile, cx: &App) -> AnyElement {
    let path = file.path().clone();
    let viewed = task.upgrade().is_some_and(|t| t.read(cx).is_path_viewed(&path));
    let task = task.clone();
    gpui_kit::component::checkbox::Checkbox::new(SharedString::from(format!("viewed-{path}")))
        .label("Viewed")
        .checked(viewed)
        .tooltip("Viewed until it changes again")
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            let _ = task.update(cx, |t, cx| t.toggle_viewed(&path, cx));
        })
        .into_any_element()
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
        return rg(&function_declarations(symbol), 5);
    }
    let (class, member) = symbol.split_once('.').map_or((symbol, None), |(c, m)| (c, Some(m)));
    let found = files
        .iter()
        .find(|f| file_name_of(f).split('.').next() == Some(class) && f.contains('.'))
        .map(|f| (root.join(f), 0u32, String::new()))
        .or_else(|| rg(&type_declarations(class), 1).into_iter().next());
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

/// Lines that declare a function or variable `name`, for ripgrep.
fn function_declarations(name: &str) -> Vec<String> {
    vec![
        format!(r"\b(fun|func|function|def|fn|val|var|const|let)\s+([A-Za-z_][\w.]*\.)?{name}\b"),
        // Go methods: func (r *T) name(
        format!(r"\bfunc\s*\([^)]*\)\s*{name}\b"),
        // Java/C-style: a type then the name then "(" at the start of a line.
        format!(r"^\s*(public|private|protected|static|final|override|suspend|internal|\s)*[\w<>\[\],.? ]+\s+{name}\s*\("),
    ]
}

/// Lines that declare a type `name`, for ripgrep.
fn type_declarations(name: &str) -> Vec<String> {
    vec![format!(r"\b(class|object|interface|struct|enum|trait|type)\s+{name}\b")]
}

/// Lines that implement or extend a type `name`, for ripgrep.
fn implementing_declarations(name: &str) -> Vec<String> {
    vec![
        // Rust: impl<T> Name<T> for Type
        format!(r"\bimpl\b[^{{;]*\b{name}\b[^{{;]*\bfor\b"),
        // Java, TypeScript, PHP: class A extends B implements Name
        format!(r"\b(implements|extends)\b[^{{]*\b{name}\b"),
        // Kotlin, Swift, C#, C++: class A : Name, after a name or a constructor.
        format!(r"\b(class|object|struct|enum|interface|record)\s+\w+\s*(<[^>]*>)?\s*:[^{{=]*\b{name}\b"),
        format!(r"\b(class|object)\s+\w+.*\)\s*:[^{{=(]*\b{name}\b"),
        // Python: class A(Name)
        format!(r"^\s*class\s+\w+\s*\([^)]*\b{name}\b"),
        // Go: a struct does not name its interface, so nothing to find there.
    ]
}

/// What Ctrl+Alt+B or Ctrl+B on a declaration asks for.
struct PlacesQuery {
    path: PathBuf,
    pos: Position,
    word: String,
    implementations: bool,
}

/// The answer, each list sorted by file and line.
#[derive(Default)]
struct Places {
    implementations: Vec<crate::finder::Hit>,
    usages: Vec<crate::finder::Hit>,
    implementations_by_search: bool,
    usages_by_search: bool,
}

/// Off the UI thread: the language server where it answers, text search
/// where it cannot (none, not started, no such request, an error). `buffer`
/// is the open file's text, which may differ from the disk.
fn find_places(
    root: &std::path::Path,
    files: &[String],
    query: &PlacesQuery,
    buffer: &str,
    client: Option<&lsp::LspClient>,
) -> Places {
    let uri = lsp::path_to_uri(&query.path);
    let ask = |provider: &str, request: &dyn Fn() -> anyhow::Result<Vec<lsp_types::Location>>| {
        let client = client?;
        if !client.supports(provider) {
            return None;
        }
        request().inspect_err(|e| eprintln!("{provider}: {e:#}")).ok()
    };
    let (implementations, usages) = std::thread::scope(|s| {
        let implementations = s.spawn(|| {
            if !query.implementations {
                return Some(Vec::new());
            }
            ask("implementationProvider", &|| client.unwrap().implementation(&uri, query.pos))
        });
        let usages = ask("referencesProvider", &|| client.unwrap().references(&uri, query.pos));
        (implementations.join().unwrap_or(None), usages)
    });

    let mut places = Places::default();
    let mut texts = TextCache::new(&query.path, buffer);
    let mut search = None;
    let mut by_search = || search.get_or_insert_with(|| search_places(root, files, &query.word, query.implementations)).clone();
    match implementations {
        Some(found) => places.implementations = found.iter().filter_map(|l| texts.hit(root, l)).collect(),
        None => {
            places.implementations = by_search().0;
            places.implementations_by_search = true;
        }
    }
    match usages {
        Some(found) => places.usages = found.iter().filter_map(|l| texts.hit(root, l)).collect(),
        None => {
            places.usages = by_search().1;
            places.usages_by_search = true;
        }
    }
    for list in [&mut places.implementations, &mut places.usages] {
        list.sort_by(|a, b| (&a.path, a.line, a.col).cmp(&(&b.path, b.line, b.col)));
        list.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.col == b.col);
    }
    // The place asked about is no answer, even when the server lists it.
    let origin = (query.path.strip_prefix(root).unwrap_or(&query.path).display().to_string(), query.pos.line + 1);
    places.implementations.retain(|h| (h.path.clone(), h.line) != origin);
    places.usages.retain(|h| (h.path.clone(), h.line) != origin);
    // Some servers list an implementation among the references too.
    let implemented: HashSet<(String, u32)> = places.implementations.iter().map(|h| (h.path.clone(), h.line)).collect();
    places.usages.retain(|h| !implemented.contains(&(h.path.clone(), h.line)));
    places
}

/// Lines of the files locations point into, read once each.
struct TextCache {
    texts: HashMap<PathBuf, Option<String>>,
}

impl TextCache {
    fn new(open: &std::path::Path, buffer: &str) -> Self {
        Self { texts: HashMap::from([(open.to_path_buf(), Some(buffer.to_string()))]) }
    }

    /// A location as a place in the list: its path relative to `root` when
    /// inside it, its line's code.
    fn hit(&mut self, root: &std::path::Path, location: &lsp_types::Location) -> Option<crate::finder::Hit> {
        let path = lsp::uri_to_path(location.uri.as_str())?;
        let start = location.range.start;
        let text = self
            .texts
            .entry(path.clone())
            .or_insert_with(|| std::fs::read_to_string(&path).ok())
            .as_deref()
            .and_then(|t| t.lines().nth(start.line as usize))
            .map(|l| l.trim().chars().take(300).collect())
            .unwrap_or_default();
        let shown = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        Some(crate::finder::Hit { path: shown, line: start.line + 1, col: start.character, text, ranges: Vec::new() })
    }
}

/// Without a language server: implementations by the declarations that name
/// the type, usages as every whole-word occurrence that is no declaration.
fn search_places(
    root: &std::path::Path,
    files: &[String],
    word: &str,
    implementations: bool,
) -> (Vec<crate::finder::Hit>, Vec<crate::finder::Hit>) {
    let rg = |patterns: &[String], per_file: u64, limit: usize| -> Vec<crate::finder::Hit> {
        let Some(matcher) = crate::search::regexes(patterns) else { return Vec::new() };
        crate::search::search(root, &matcher, per_file, 300, limit)
            .into_iter()
            .map(|m| {
                let col = m.text.find(word).map_or(0, |b| m.text[..b].chars().count() as u32);
                crate::finder::Hit { path: m.path.display().to_string(), line: m.line as u32, col, text: m.text.trim().to_string(), ranges: Vec::new() }
            })
            .collect()
    };
    let is_type = word.chars().next().is_some_and(|c| c.is_uppercase());
    let found_implementations = if implementations && is_type { rg(&implementing_declarations(word), 20, 200) } else { Vec::new() };
    let mut declarations: Vec<String> = function_declarations(word);
    declarations.extend(type_declarations(word));
    let declared: HashSet<(String, u32)> = rg(&declarations, 20, 200)
        .into_iter()
        .chain(find_symbol(root, files, word).into_iter().map(|(path, line, _)| crate::finder::Hit {
            path: path.strip_prefix(root).unwrap_or(&path).display().to_string(),
            line: line + 1,
            col: 0,
            text: String::new(),
            ranges: Vec::new(),
        }))
        .chain(found_implementations.iter().cloned())
        .map(|h| (h.path, h.line))
        .collect();
    let usages = rg(&[format!(r"\b{word}\b")], 100, 1000)
        .into_iter()
        .filter(|h| !declared.contains(&(h.path.clone(), h.line)))
        .collect();
    (found_implementations, usages)
}

/// The location `UsagesAtDeclaration` hands the editor for "show usages".
const USAGES_URI: &str = "ideae-usages:here";

/// Ctrl+B and Ctrl+click on a declaration itself list its usages, as in
/// IntelliJ, rather than jump to where the cursor already is. The editor
/// follows whatever location it is given, so the request travels as a
/// location of its own (`USAGES_URI`), which `show_document_handler` takes.
struct UsagesAtDeclaration {
    inner: Rc<dyn DefinitionProvider>,
    path: PathBuf,
}

impl UsagesAtDeclaration {
    fn wrap(inner: Rc<dyn DefinitionProvider>, path: &std::path::Path) -> Rc<dyn DefinitionProvider> {
        Rc::new(Self { inner, path: path.to_path_buf() })
    }
}

/// Whether `target` is where `pos` is: inside the range, or on its line for
/// a search result, which has a line only.
fn is_at(target: lsp_types::Range, pos: Position) -> bool {
    if target.start == target.end {
        return target.start.line == pos.line;
    }
    let key = |p: Position| (p.line, p.character);
    key(target.start) <= key(pos) && key(pos) <= key(target.end)
}

impl DefinitionProvider for UsagesAtDeclaration {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<lsp_types::LocationLink>>> {
        let pos = text.offset_to_position(offset);
        let found = self.inner.definitions(text, offset, window, cx);
        let path = self.path.clone();
        cx.spawn(async move |_| {
            let links = found.await?;
            let here = |l: &lsp_types::LocationLink| {
                lsp::uri_to_path(l.target_uri.as_str()).as_deref() == Some(path.as_path()) && is_at(l.target_selection_range, pos)
            };
            if links.is_empty() || !links.iter().all(here) {
                return Ok(links);
            }
            let at = lsp_types::Range { start: pos, end: pos };
            Ok(vec![lsp_types::LocationLink {
                origin_selection_range: links[0].origin_selection_range,
                target_uri: USAGES_URI.parse()?,
                target_range: at,
                target_selection_range: at,
            }])
        })
    }
}

/// Ctrl+click and Ctrl+B in the editor when no language server runs: the
/// word under the cursor (with `Class.` before it, if any) by `find_symbol`.
struct SearchDefinitionProvider {
    root: PathBuf,
    files: Arc<std::sync::RwLock<Arc<Vec<String>>>>,
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
        if params.uri.as_str() == USAGES_URI {
            let pos = params.selection.map(|r| r.start).unwrap_or_default();
            let (path, task) = (this_path.clone(), task.clone());
            // Called while the editor is being updated, and the list reads it.
            window.defer(cx, move |window, cx| {
                let _ = task.update(cx, |task, cx| task.show_places(path, pos, false, window, cx));
            });
            return true;
        }
        let Some(target) = lsp::uri_to_path(params.uri.as_str()) else {
            return false;
        };
        if target == this_path {
            // The editor moves the cursor itself; this is where it was.
            let _ = task.update(cx, |task, cx| task.record_nav(cx));
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
                    TabKind::Terminal(view) if view.read(cx).service.is_some() => {
                        s.kind = "service".into();
                        s.path = view.read(cx).service.clone();
                    }
                    TabKind::Terminal(view) => {
                        s.kind = "shell".into();
                        s.line = view.read(cx).seq as u32;
                    }
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
            review: self.changes.read(cx).review.clone(),
            viewed: self.viewed.clone(),
            side_panel: match self.right {
                RightPane::Changes => 0,
                RightPane::Files => 1,
                RightPane::Structure => 2,
                RightPane::Run => 3,
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
                // `line` holds the shell's number, for its session.
                "shell" if saved.line > 0 => {
                    self.terminal_tab(saved.line as usize, window, cx);
                }
                "shell" => {
                    self.new_terminal_tab(window, cx);
                }
                // Attaches to it in the daemon, or starts it again.
                "service" => {
                    if let Some(name) = &saved.path {
                        self.service_tab(name, window, cx);
                    }
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
        self.viewed = session.viewed.clone();
        let review = session.review.clone();
        self.changes.update(cx, |c, cx| {
            c.review = review;
            cx.notify();
        });
        self.right = match session.side_panel {
            1 => RightPane::Files,
            2 => RightPane::Structure,
            3 => RightPane::Run,
            _ => RightPane::Changes,
        };
        self.structure_file = self.focused_ix().filter(|&ix| matches!(self.tabs[ix].kind, TabKind::File(_))).map(|ix| self.tabs[ix].id);
        self.sync_visibility(cx);
        cx.notify();
    }

    /// The daemon sessions of this task's terminals.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn sessions(&self, cx: &App) -> Vec<String> {
        self.tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::Terminal(view) => view.read(cx).session().map(str::to_string),
                _ => None,
            })
            .collect()
    }

    /// The agent is in the middle of a turn.
    pub fn agent_busy(&self) -> bool {
        self.agent_status == crate::agent::AgentStatus::Working
    }

    /// Stops every terminal and browser of the task before it is dropped,
    /// and what they started that left their sessions.
    pub fn shut_down(&mut self, cx: &mut Context<Self>) {
        self.end(true, cx);
    }

    /// As `shut_down` when the app quits: it looks for what every task
    /// left behind once, itself.
    pub fn shut_down_at_quit(&mut self, cx: &mut Context<Self>) {
        self.end(false, cx);
    }

    fn end(&mut self, strays: bool, cx: &mut Context<Self>) {
        self.save_all_now(cx);
        let task = self.root.display().to_string();
        let socket = self.hook_socket.clone().filter(|_| strays);
        // Before the terminals go: what they started is told by them.
        let seen = socket.as_ref().map(|socket| crate::procs::task_processes(Some(&task), socket)).unwrap_or_default();
        for tab in &self.tabs {
            if let TabKind::Terminal(view) = &tab.kind {
                TermView::end(&view, cx);
            }
        }
        if let Some(socket) = socket {
            std::thread::spawn(move || crate::procs::end_task_strays(Some(&task), &socket, seen));
        }
    }
}

// ---------------------------------------------------------------- power saving

impl TaskView {
    /// Definitions by search, so Ctrl+click and Ctrl+B work without a
    /// language server; `replace` drops the server's providers first.
    fn use_search_definitions(&self, path: &PathBuf, editor: &Entity<EditorState>, replace: bool, cx: &mut Context<Self>) {
        let search = UsagesAtDeclaration::wrap(
            Rc::new(SearchDefinitionProvider { root: self.root.clone(), files: self.shared_files.clone() }),
            path,
        );
        let task_weak = cx.weak_entity();
        let this_path = path.clone();
        editor.update(cx, |state, cx| {
            let lsp = state.lsp_mut();
            if replace {
                // All of them: one left would keep the ended server's client,
                // and ask it on every key.
                lsp.hover_provider = None;
                lsp.definition_provider = None;
                lsp.completion_provider = None;
                lsp.code_action_providers.clear();
                lsp.semantic_tokens_provider = None;
            }
            if lsp.definition_provider.is_none() {
                lsp.definition_provider = Some(search);
                lsp.show_document = Some(show_document_handler(task_weak, this_path));
            }
            cx.notify();
        });
    }

    /// Stops this task's language servers, or starts them again for the
    /// open files.
    pub fn set_language_servers(&mut self, on: bool, cx: &mut Context<Self>) {
        let status = if on || !self.lsp_asleep { "language servers off (power saving)" } else { ASLEEP };
        // Asleep, a task starts its servers when it comes into view, not now.
        if on && self.lsp_asleep {
            return;
        }
        let files: Vec<Entity<FileView>> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::File(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        if !on {
            for client in self.lsp.values() {
                client.shut_down();
            }
            self.lsp.clear();
        }
        for file in files {
            let (path, editor) = {
                let f = file.read(cx);
                (f.path.clone(), f.editor.clone())
            };
            if on {
                let content = editor.read(cx).value().to_string();
                self.attach_lsp(&path, content, &editor, &file, cx);
            } else {
                file.update(cx, |f, cx| {
                    f.lsp_client = None;
                    f.lsp_program.clear();
                    f.lsp_ok = Some(false);
                    f.lsp_missing = None;
                    f.lsp_status = status.into();
                    cx.notify();
                });
                self.use_search_definitions(&path, &editor, true, cx);
            }
        }
        cx.notify();
    }
}

const ASLEEP: &str = "language server asleep: task out of view";

impl TaskView {
    /// Stops the language servers of a task out of view; they cost
    /// gigabytes each, and reindexing on the way back takes a minute or two.
    pub fn put_language_servers_to_sleep(&mut self, cx: &mut Context<Self>) {
        if self.lsp_asleep {
            return;
        }
        self.lsp_asleep = true;
        self.set_language_servers(false, cx);
    }

    pub fn wake_language_servers(&mut self, cx: &mut Context<Self>) {
        if !self.lsp_asleep {
            return;
        }
        self.lsp_asleep = false;
        // Under power saving the files say so rather than "asleep".
        let on = !lsp::POWER_SAVING.load(std::sync::atomic::Ordering::Relaxed);
        self.set_language_servers(on, cx);
    }

    /// Ends the servers no open file has used for `idle`: one runs on for a
    /// while after its last file closes, since the next file of the
    /// language would have it index the project again.
    pub fn drop_unused_language_servers(&mut self, idle: Duration, cx: &App) {
        let used: Vec<Arc<lsp::LspClient>> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::File(f) => f.read(cx).lsp_client.clone(),
                _ => None,
            })
            .collect();
        let now = std::time::Instant::now();
        let mut over = Vec::new();
        for (key, client) in &self.lsp {
            if used.iter().any(|u| Arc::ptr_eq(u, client)) {
                self.lsp_unused.remove(key);
            } else if now.duration_since(*self.lsp_unused.entry(key.clone()).or_insert(now)) >= idle {
                over.push(key.clone());
            }
        }
        for key in over {
            self.lsp_unused.remove(&key);
            if let Some(client) = self.lsp.remove(&key) {
                client.shut_down();
            }
        }
    }

    pub fn has_language_servers(&self) -> bool {
        !self.lsp.is_empty()
    }
}

// ---------------------------------------------------------------- changes tree

#[derive(Debug, PartialEq)]
enum ChangeNode {
    /// A folder; chains of folders holding one folder and no files show as
    /// one ("src/main/kotlin").
    Dir { path: String, label: String, depth: usize },
    File { ix: usize, depth: usize },
}

/// Sorted paths as rows of a folder tree, folders first.
fn change_tree(paths: &[(usize, &str)]) -> Vec<ChangeNode> {
    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        files: Vec<(String, usize)>,
    }
    let mut root = Node::default();
    for &(ix, path) in paths {
        let mut node = &mut root;
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap_or_default();
        for part in parts {
            node = node.dirs.entry(part.to_string()).or_default();
        }
        node.files.push((name.to_string(), ix));
    }
    fn walk(node: &Node, prefix: &str, depth: usize, out: &mut Vec<ChangeNode>) {
        for (name, child) in &node.dirs {
            let (mut label, mut path, mut child) = (name.clone(), format!("{prefix}{name}"), child);
            while child.files.is_empty() && child.dirs.len() == 1 {
                let (next, grandchild) = child.dirs.iter().next().unwrap();
                label = format!("{label}/{next}");
                path = format!("{path}/{next}");
                child = grandchild;
            }
            out.push(ChangeNode::Dir { path: path.clone(), label, depth });
            walk(child, &format!("{path}/"), depth + 1, out);
        }
        let mut files = node.files.clone();
        files.sort();
        for (_, ix) in files {
            out.push(ChangeNode::File { ix, depth });
        }
    }
    let mut out = Vec::new();
    walk(&root, "", 0, &mut out);
    out
}

#[cfg(test)]
mod change_tree_tests {
    use super::{ChangeNode, change_tree};

    #[test]
    fn folders_compact_and_nest() {
        let paths = [(0, "README.md"), (1, "src/main/kotlin/App.kt"), (2, "src/main/kotlin/ui/View.kt"), (3, "src/test/T.kt")];
        let dir = |path: &str, label: &str, depth| ChangeNode::Dir { path: path.into(), label: label.into(), depth };
        assert_eq!(
            change_tree(&paths),
            vec![
                dir("src", "src", 0),
                dir("src/main/kotlin", "main/kotlin", 1),
                dir("src/main/kotlin/ui", "ui", 2),
                ChangeNode::File { ix: 2, depth: 3 },
                ChangeNode::File { ix: 1, depth: 2 },
                dir("src/test", "test", 1),
                ChangeNode::File { ix: 3, depth: 2 },
                ChangeNode::File { ix: 0, depth: 0 },
            ]
        );
    }
}

fn location_of(range: &DiffLineRange) -> String {
    if range.start() == range.end() {
        format!("{}:{}", range.path(), range.start())
    } else {
        format!("{}:{}-{}", range.path(), range.start(), range.end())
    }
}

// ---------------------------------------------------------------- services

use gpui_kit::component::notification::Notification;

impl TaskView {
    fn service_ix(&self, name: &str, _: &App) -> Option<usize> {
        self.tabs.iter().position(|t| matches!(t.kind, TabKind::Terminal(_)) && t.service.as_deref() == Some(name))
    }

    /// The services running now, with their ports, for the task's row.
    pub fn running_services(&self, cx: &App) -> Vec<(String, Option<u16>)> {
        self.services
            .iter()
            .filter(|s| self.service_ix(&s.name, cx).is_some())
            .map(|s| (s.name.clone(), s.port(self.port_slot)))
            .collect()
    }

    /// The service's terminal tab, created when missing; returns its index.
    fn service_tab(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        if let Some(ix) = self.service_ix(name, cx) {
            return Some(ix);
        }
        let service = self.services.iter().find(|s| s.name == name)?.clone();
        let slot = self.port_slot;
        let mut env = crate::services::task_env(&self.services, slot);
        env.extend(service.env(&self.services, slot));
        let command = service.expand(&service.command, &self.services, slot);
        let cwd = service.cwd.as_ref().map_or_else(|| self.root.clone(), |c| self.root.join(c));
        self.terminal_seq += 1;
        let seq = self.terminal_seq;
        let tab = Self::make_terminal(
            &self.root,
            self.hook_socket.as_ref(),
            &format!("▶ {name}"),
            seq,
            Some(command),
            env,
            Some((name.to_string(), cwd)),
            window,
            cx,
        );
        let at = self.tabs.iter().rposition(|t| matches!(t.kind, TabKind::Terminal(_))).map_or(0, |ix| ix + 1);
        self.tabs.insert(at, tab);
        Some(at)
    }

    fn start_service(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.reload_services() {
            window.push_notification(Notification::error(e).title("Services"), cx);
            return;
        }
        let port = self.services.iter().find(|s| s.name == name).and_then(|s| s.port(self.port_slot));
        if self.service_ix(name, cx).is_none() {
            if let Some(owner) = port.and_then(crate::services::port_taken) {
                window.push_notification(
                    Notification::warning(format!("Port {} is in use by {owner}.", port.unwrap_or(0))).title(format!("{name} not started")),
                    cx,
                );
                return;
            }
        }
        if let Some(ix) = self.service_tab(name, window, cx) {
            self.activate(ix, window, cx);
        }
        cx.emit(TaskEvent::Changed);
    }

    /// Every service of the task, started behind what is in front. One
    /// whose port something listens on already is left: the agent may have
    /// started it itself. How many were started.
    pub fn start_services(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        if self.reload_services().is_err() {
            return 0;
        }
        let waiting: Vec<String> = self
            .services
            .iter()
            .filter(|s| self.service_ix(&s.name, cx).is_none() && s.port(self.port_slot).and_then(crate::services::port_taken).is_none())
            .map(|s| s.name.clone())
            .collect();
        let started = waiting.into_iter().filter(|name| self.service_tab(name, window, cx).is_some()).count();
        cx.emit(TaskEvent::Changed);
        cx.notify();
        started
    }

    pub fn stop_services(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (name, _) in self.running_services(cx) {
            self.stop_service(&name, window, cx);
        }
    }

    pub fn has_services(&self) -> bool {
        !self.services.is_empty()
    }

    /// The task's ports as its terminals have them, for what else runs in it.
    pub fn service_env(&self) -> Vec<(String, String)> {
        crate::services::task_env(&self.services, self.port_slot)
    }

    /// Where the task's site is: the first service that has an address.
    pub fn site(&self) -> Option<String> {
        self.services.iter().find_map(|s| s.url(&self.services, self.port_slot))
    }

    /// The page in front, when it is one of the task's own site: what
    /// follows its address, to show the same page of another variant.
    pub fn shown_page(&self, cx: &App) -> Option<String> {
        let TabKind::Browser(view) = &self.tabs.get(self.focused_ix()?)?.kind else { return None };
        let (url, site) = (view.read(cx).url(), self.site()?);
        url.strip_prefix(origin(&site)).map(str::to_string)
    }

    /// The task's site in a browser tab, at `page` of it when given: the
    /// tab that is on the site already, or a new one.
    pub fn show_site(&mut self, page: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(site) = self.site() else { return };
        let url = match page {
            Some(page) => format!("{}{page}", origin(&site)),
            None => site.clone(),
        };
        let open = self.tabs.iter().position(|t| match &t.kind {
            TabKind::Browser(view) => view.read(cx).url().starts_with(origin(&site)),
            _ => false,
        });
        match open {
            Some(ix) => {
                if let TabKind::Browser(view) = &self.tabs[ix].kind {
                    if view.read(cx).url() != url {
                        view.update(cx, |v, _| v.go(&url));
                    }
                }
                self.activate(ix, window, cx);
            }
            None => self.open_browser(&url, window, cx),
        }
    }

    fn stop_service(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.service_ix(name, cx) else { return };
        if let TabKind::Terminal(view) = &self.tabs[ix].kind {
            TermView::end(&view.clone(), cx);
        }
        self.remove_tab(ix, window, cx);
        cx.emit(TaskEvent::Changed);
    }

    fn open_service(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.services.iter().find(|s| s.name == name).and_then(|s| s.url(&self.services, self.port_slot));
        if let Some(url) = url {
            self.open_browser(&url, window, cx);
        }
    }

    fn reload_services(&mut self) -> Result<(), String> {
        self.services = crate::services::load(&self.root)?;
        Ok(())
    }

    /// Writes a commented `.ideae.toml` to start from, and opens it.
    fn create_services_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.root.join(crate::services::FILE);
        if !path.exists() {
            if let Err(e) = std::fs::write(&path, crate::services::TEMPLATE) {
                window.push_notification(Notification::error(e.to_string()).title("Services"), cx);
                return;
            }
        }
        self.open_file(path, 0, window, cx);
    }

    /// Every few seconds while the Run panel is in view: the ports the
    /// task's processes listen on.
    fn watch_listening(&mut self, cx: &mut Context<Self>) {
        let Some(socket) = self.hook_socket.clone() else { return };
        let root = self.root.display().to_string();
        self._listening_poll = Some(cx.spawn(async move |this, cx| {
            loop {
                let (root, socket) = (root.clone(), socket.clone());
                let ports = cx
                    .background_spawn(async move {
                        crate::services::listening(&crate::procs::task_processes(Some(&root), &socket))
                    })
                    .await;
                let shown = this.update(cx, |t, cx| {
                    if t.listening != ports {
                        t.listening = ports;
                        cx.notify();
                    }
                    t.right == RightPane::Run && t.visible
                });
                if !matches!(shown, Ok(true)) {
                    let _ = this.update(cx, |t, _| t._listening_poll = None);
                    return;
                }
                cx.background_executor().timer(Duration::from_secs(3)).await;
            }
        }));
    }

    /// Rereads `.ideae.toml` and, without one, looks for what to suggest.
    fn refresh_services(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let (loaded, found) = cx.background_spawn(async move { (crate::services::load(&root), crate::services::detect(&root)) }).await;
            let _ = this.update(cx, |this, cx| {
                match loaded {
                    Ok(services) => {
                        this.services = services;
                        this.services_error = None;
                    }
                    Err(e) => this.services_error = Some(e),
                }
                let known: HashSet<String> = this.services.iter().map(|s| s.name.clone()).collect();
                this.services_found = found.into_iter().filter(|f| !known.contains(&f.service.name)).collect();
                cx.notify();
            });
        })
        .detach();
    }

    fn add_found_service(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(found) = self.services_found.get(ix).cloned() else { return };
        if let Err(e) = crate::services::add(&self.root, &found.service) {
            self.services_error = Some(e);
        }
        self.services_found.remove(ix);
        if let Ok(services) = crate::services::load(&self.root) {
            self.services = services;
        }
        cx.notify();
    }

    /// What the project seems to run and `.ideae.toml` lacks, each with Add.
    fn render_found(&self, mut panel: Stateful<Div>, heading: &'static str, cx: &mut Context<Self>) -> Stateful<Div> {
        if self.services_found.is_empty() {
            return panel;
        }
        let muted = cx.theme().muted_foreground;
        panel = panel.child(div().px_3().pt_2().text_xs().text_color(muted).child(heading));
        for (ix, found) in self.services_found.iter().enumerate() {
            let s = &found.service;
            panel = panel.child(
                h_flex()
                    .id(("found", ix))
                    .px_3()
                    .py_1()
                    .gap_2()
                    .hover(|d| d.bg(cx.theme().accent))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(h_flex().gap_2().child(s.name.clone()).when_some(s.port, |row, p| {
                                row.child(div().text_xs().text_color(muted).child(format!(":{p}")))
                            }))
                            .child(div().text_xs().text_color(muted).truncate().child(format!("{} · {}", s.command, found.source))),
                    )
                    .child(
                        Button::new(("add-found", ix))
                            .xsmall()
                            .outline()
                            .label("Add")
                            .tooltip(format!("Write it to {} at the repository root", crate::services::FILE))
                            .on_click(cx.listener(move |this, _, _, cx| this.add_found_service(ix, cx))),
                    ),
            );
        }
        panel
    }

    /// Ports something of the task listens on that no service describes.
    fn render_listening(&self, mut panel: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        let described: Vec<u16> = self.services.iter().filter_map(|s| s.port(self.port_slot)).collect();
        let ports: Vec<_> = self.listening.iter().filter(|l| !described.contains(&l.port)).cloned().collect();
        if ports.is_empty() {
            return panel;
        }
        let muted = cx.theme().muted_foreground;
        panel = panel.child(div().px_3().pt_2().text_xs().text_color(muted).child("LISTENING"));
        for l in ports {
            let url = format!("http://localhost:{}", l.port);
            panel = panel.child(
                h_flex()
                    .id(("listening", l.port as usize))
                    .px_3()
                    .py_1()
                    .gap_2()
                    .hover(|d| d.bg(cx.theme().accent))
                    .child(div().text_color(gpui_kit::green()).child("●"))
                    .child(div().child(format!(":{}", l.port)))
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(format!("{} {}", l.name, l.pid)))
                    .child(
                        Button::new(("listening-open", l.port as usize))
                            .xsmall()
                            .ghost()
                            .label("open")
                            .tooltip(format!("Open {url} in a Web tab"))
                            .on_click(cx.listener(move |this, _, window, cx| this.open_browser(&url, window, cx))),
                    ),
            );
        }
        panel
    }

    fn render_run_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mut panel = v_flex().id("run-panel").flex_1().min_h_0().overflow_y_scroll().text_sm();
        if let Some(error) = &self.services_error {
            panel = panel.child(div().px_3().py_2().text_xs().text_color(gpui_kit::red()).child(error.clone()));
        }
        if self.services.is_empty() {
            panel = panel.child(
                v_flex()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .child(div().font_semibold().child("Services"))
                    .child(div().text_xs().text_color(muted).child(
                        "Dev servers and workers this task runs. Each task gets its own ports, \
                         so several branches run side by side; its terminals, the agent's too, \
                         get them as IDEAE_PORT_<NAME>.",
                    )),
            );
            panel = self.render_listening(panel, cx);
            panel = self.render_found(panel, "FOUND IN THE PROJECT", cx);
            return panel
                .child(
                    v_flex()
                        .gap_1()
                        .px_3()
                        .py_3()
                        .child(
                            Button::new("describe-services")
                                .small()
                                .ghost()
                                .label(format!("Describe by hand in {}…", crate::services::FILE))
                                .on_click(cx.listener(|this, _, window, cx| this.create_services_file(window, cx))),
                        )
                        .child(div().text_xs().text_color(muted).child(format!(
                            "{} goes to the repository root; commit it so every task has the same services.",
                            crate::services::FILE
                        ))),
                )
                .into_any_element();
        }
        let running: Vec<String> = self.running_services(cx).into_iter().map(|(n, _)| n).collect();
        for (ix, service) in self.services.iter().enumerate() {
            let name = service.name.clone();
            let on = running.contains(&name);
            let port = service.port(self.port_slot);
            let command = service.expand(&service.command, &self.services, self.port_slot);
            let action = |id: &'static str, label: &'static str, tip: String, f: fn(&mut TaskView, &str, &mut Window, &mut Context<TaskView>)| {
                let name = name.clone();
                Button::new((id, ix)).xsmall().ghost().label(label).tooltip(tip).on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    f(this, &name, window, cx);
                    cx.notify();
                }))
            };
            panel = panel.child(
                h_flex()
                    .id(("service", ix))
                    .px_3()
                    .py_1()
                    .gap_2()
                    .hover(|d| d.bg(cx.theme().accent))
                    .child(div().text_color(if on { gpui_kit::green() } else { muted }).child(if on { "●" } else { "○" }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(h_flex().gap_2().child(name.clone()).when_some(port, |row, p| {
                                row.child(div().text_xs().text_color(muted).child(format!(":{p}")))
                            }))
                            .child(div().text_xs().text_color(muted).truncate().child(command)),
                    )
                    .when(on, |row| {
                        row.child(action("svc-logs", "logs", "Its terminal".into(), |t, n, w, cx| {
                            if let Some(ix) = t.service_ix(n, cx) {
                                t.activate(ix, w, cx);
                            }
                        }))
                        .child(action("svc-restart", "↻", "Restart".into(), |t, n, w, cx| {
                            t.stop_service(n, w, cx);
                            t.start_service(n, w, cx);
                        }))
                        .child(action("svc-stop", "■", "Stop".into(), TaskView::stop_service))
                    })
                    .when(!on, |row| row.child(action("svc-start", "▶", "Start".into(), TaskView::start_service)))
                    .when(service.url.is_some(), |row| {
                        let url = service.url(&self.services, self.port_slot).unwrap_or_default();
                        row.child(action("svc-open", "open", format!("Open {url} in a Web tab"), TaskView::open_service))
                    }),
            );
        }
        panel = self.render_listening(panel, cx);
        panel = self.render_found(panel, "ALSO IN THE PROJECT", cx);
        panel
            .child(
                h_flex().px_3().py_2().child(
                    Button::new("edit-services")
                        .xsmall()
                        .ghost()
                        .label(format!("Edit {}", crate::services::FILE))
                        .on_click(cx.listener(|this, _, window, cx| this.create_services_file(window, cx))),
                ),
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------- PR feedback

impl TaskView {
    /// "PR #12: 3 comments → agent", while reviewers left something open.
    fn render_pr_feedback(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pr = self.pr.clone().filter(|p| p.open_feedback > 0)?;
        let n = pr.open_feedback;
        Some(
            Button::new("pr-feedback")
                .xsmall()
                .ghost()
                .disabled(self.busy.is_some())
                .label(format!("PR #{}: {n} comment{} → agent", pr.number, if n == 1 { "" } else { "s" }))
                .tooltip("Put the unresolved review threads and change requests into the agent's prompt")
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.send_pr_feedback(pr.clone(), window, cx)
                }))
                .into_any_element(),
        )
    }

    fn send_pr_feedback(&mut self, pr: crate::pr::Pr, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("PR feedback");
        cx.notify();
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let job_pr = pr.clone();
            let feedback = cx.background_spawn(crate::procs::on_thread(move || { crate::pr::feedback(&root, &job_pr) })).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match feedback {
                    Some(f) if f.len() > 0 => {
                        this.paste_to_agent(f.message(&pr), window, cx);
                        this.action = Some((true, format!("{} review point(s) in the agent's prompt", f.len())));
                    }
                    Some(_) => this.action = Some((true, "No open review comments".into())),
                    None => this.action = Some((false, "PR feedback: gh could not read the review".into())),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// `file` as the panels name it: relative to the worktree, with slashes.
/// The agent may name the worktree through another path than the app does
/// (a symlink on the way), so the real paths are compared too.
fn path_in(root: &std::path::Path, file: &std::path::Path) -> Option<String> {
    let real = |path: &std::path::Path| match (path.parent(), path.file_name()) {
        // The file itself may not be there yet.
        (Some(parent), Some(name)) => parent.canonicalize().ok().map(|parent| parent.join(name)),
        _ => None,
    };
    let rel = match file.strip_prefix(root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => real(file)?.strip_prefix(root.canonicalize().ok()?).ok()?.to_path_buf(),
    };
    let parts: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod path_in_tests {
    use std::path::Path;

    use super::path_in;

    #[test]
    fn relative_to_the_worktree_also_through_a_symlink() {
        assert_eq!(path_in(Path::new("/w/t"), Path::new("/w/t/src/a.rs")).as_deref(), Some("src/a.rs"));
        assert_eq!(path_in(Path::new("/w/t"), Path::new("/w/other/a.rs")), None);
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir().join(format!("ideae-path-in-{}", std::process::id()));
            let (real, link) = (dir.join("real"), dir.join("link"));
            std::fs::create_dir_all(real.join("src")).unwrap();
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert_eq!(path_in(&link, &real.join("src/new.rs")).as_deref(), Some("src/new.rs"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
mod disk_tests {
    use super::{DiskChange, Rope, changed_on_disk, mtime, parse_patch, same_text, text_hash, write_in_order};

    #[test]
    fn the_editor_text_is_compared_in_place() {
        // Long enough to be in several chunks of the rope.
        let text = "fn main() { println!(\"привет\"); }\n".repeat(4000);
        let rope = Rope::from(text.as_str());
        assert!(same_text(&rope, &text));
        assert!(!same_text(&rope, &text.replacen("main", "mine", 1)), "the same length, another text");
        assert!(!same_text(&rope, &text[..text.len() - 1]));
        assert!(same_text(&Rope::from(""), ""));
    }

    #[test]
    fn writes_keep_their_order_and_changes_on_disk_are_read() {
        let dir = std::env::temp_dir().join(format!("ideae-disk-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        let gate = std::sync::Mutex::new(0);
        let written = write_in_order(&gate, 2, &file, b"later").unwrap();
        assert_eq!(written, mtime(&file));
        // A write asked for earlier that only starts now.
        write_in_order(&gate, 1, &file, b"earlier").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "later");

        let gone = dir.join("gone.txt");
        let stale = written.map(|t| t - std::time::Duration::from_secs(5));
        let found = changed_on_disk(vec![(file.clone(), written), (file.clone(), stale), (file.clone(), None), (gone, None)]);
        assert!(found[0].is_none(), "as known");
        assert!(matches!(&found[1], Some(DiskChange { known, now, content }) if *known == stale && Some(*now) == written && content == "later"));
        assert!(found[2].is_some() && found[3].is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_patch_is_parsed_once() {
        let patch = "diff --git a/a.txt b/a.txt\nindex 5626abf..f719efd 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1,2 @@\n one\n+two\n";
        let mut seen = text_hash("");
        assert!(parse_patch(String::new(), &mut seen).is_none(), "nothing changed, as at the start");
        let parsed = parse_patch(patch.into(), &mut seen).unwrap();
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.first_lines.get("a.txt"), Some(&1));
        assert!(parsed.hashes.contains_key("a.txt"));
        assert!(parse_patch(patch.into(), &mut seen).is_none(), "the same again");
        assert!(parse_patch(String::new(), &mut seen).is_some_and(|p| p.files.is_empty()), "committed: no changes left");
    }
}

#[cfg(all(test, unix))]
mod open_file_scenario {
    use super::{FileView, TabKind, TaskView};
    use crate::state::{ProjectState, State};
    use crate::workspace::Workspace;
    use gpui_kit::{AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds, WindowOptions, px, size};
    use std::time::Duration;

    /// A file open in a task of a real repository: typing, saving, the
    /// agent writing it meanwhile, the app quitting mid-save.
    #[gpui_kit::test]
    #[ignore]
    fn an_open_file_is_saved_and_follows_the_disk(cx: &mut TestAppContext) {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        // The task's terminal is real: its threads wake the app when they like.
        cx.executor().allow_parking();
        let dir = std::env::temp_dir().join(format!("ideae-open-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = "git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t; echo one > f.txt; git add f.txt; git commit -qm base";
        assert!(std::process::Command::new("sh").arg("-ec").arg(script).current_dir(&dir).status().unwrap().success());
        let repo = dir.join("repo").canonicalize().unwrap();
        let path = repo.join("f.txt");
        // SAFETY: set before anything of the app reads them.
        unsafe {
            std::env::set_var("IDEAE_STATE", dir.join("state.json"));
            std::env::set_var("IDEAE_NO_DAEMON", "1");
        }
        let saved = State { projects: vec![ProjectState { path: repo.clone(), ..Default::default() }], ..Default::default() };
        std::fs::write(dir.join("state.json"), serde_json::to_string(&saved).unwrap()).unwrap();
        crate::state::set_settings(crate::state::Settings { notifications: false, pr_status: false, ..Default::default() });
        cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_cn::init(cx);
            crate::theme::init(cx);
        });
        let (handle, workspace) = cx.update(|cx| {
            let bounds = Bounds { origin: Point::default(), size: size(px(1300.), px(800.)) };
            let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(Vec::new(), Some(None), None, window, cx))).expect("a test window")
        });
        cx.update_window(handle.into(), |_, window, cx| workspace.update(cx, |w, cx| w.open_main(0, window, cx))).unwrap();
        cx.run_until_parked();
        let task: Entity<TaskView> = workspace.update(cx, |w, _| w.active_view().expect("the main checkout in view"));
        cx.update_window(handle.into(), |_, window, cx| task.update(cx, |t, cx| t.open_file(path.clone(), 0, window, cx))).unwrap();
        let file: Entity<FileView> = task.update(cx, |t, _| {
            t.tabs.iter().find_map(|tab| match &tab.kind {
                TabKind::File(f) => Some(f.clone()),
                _ => None,
            })
            .expect("the file's tab")
        });
        let typed = |cx: &mut TestAppContext, text: &str| {
            cx.update_window(handle.into(), |_, window, cx| file.update(cx, |f, cx| f.editor.update(cx, |e, cx| e.insert(text, window, cx)))).unwrap();
            cx.run_until_parked();
        };
        // The refresh a file event starts, and its pause for more of them.
        let refreshed = |cx: &mut TestAppContext| {
            task.update(cx, |t, _| t.refresh_tx.try_send(()).unwrap());
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
        };
        let on_disk = || std::fs::read_to_string(&path).unwrap();
        let shown = |cx: &mut TestAppContext| file.update(cx, |f, cx| f.editor.read(cx).value().to_string());

        typed(cx, "zero\n");
        file.update(cx, |f, cx| {
            assert!(f.dirty, "typed in");
            f.save(cx);
            assert!(f.dirty && f.saving.is_some() && f.disk_known().is_none(), "being written, off this thread");
        });
        assert_eq!(on_disk(), "one\n");
        cx.run_until_parked();
        assert_eq!(on_disk(), "zero\none\n");
        file.update(cx, |f, _| assert!(!f.dirty && f.saving.is_none() && f.error.is_none() && f.disk_mtime == super::mtime(&path)));
        refreshed(cx);
        task.update(cx, |t, _| {
            assert_eq!(t.files.len(), 1, "one changed file");
            assert_eq!((t.uncommitted_diff.len(), t.uncommitted_lines.get("f.txt")), (1, Some(&1)));
        });
        assert_eq!(shown(cx), "zero\none\n", "its own save is no change on disk");

        // The agent writes the file: a clean buffer follows.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, "agent\n").unwrap();
        refreshed(cx);
        assert_eq!(shown(cx), "agent\n");
        file.update(cx, |f, _| assert!(!f.dirty && !f.conflict));

        // And again while there are edits: a choice, and neither side lost.
        typed(cx, "mine ");
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, "theirs\n").unwrap();
        refreshed(cx);
        file.update(cx, |f, cx| {
            assert!(f.dirty && f.conflict);
            f.autosave(cx);
            f.save_now(cx);
        });
        cx.run_until_parked();
        assert_eq!((shown(cx), on_disk()), ("agent\nmine ".to_string(), "theirs\n".to_string()));

        // Quitting right after a save, with more typed since: all of it is on disk at once.
        file.update(cx, |f, cx| {
            f.conflict = false;
            f.save(cx);
        });
        cx.update_window(handle.into(), |_, window, cx| file.update(cx, |f, cx| f.editor.update(cx, |e, cx| e.insert("and ", window, cx)))).unwrap();
        file.update(cx, |f, cx| f.save_now(cx));
        let last = shown(cx);
        assert!(last.contains("and ") && last.contains("mine "));
        assert_eq!(on_disk(), last);
        cx.run_until_parked();
        assert_eq!(on_disk(), last, "the earlier write does not come after");
        file.update(cx, |f, _| assert!(!f.dirty && f.saving.is_none()));

        task.update(cx, |t, cx| t.shut_down(cx));
        cx.update_window(handle.into(), |_, window, _| window.remove_window()).ok();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
