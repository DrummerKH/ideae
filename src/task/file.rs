use super::*;

// ---------------------------------------------------------------- file

actions!(ideae_git, [ShowBranches]);
actions!(ideae_file, [SaveFile, ExpandSelection, ShrinkSelection, ToggleComment, ShowOutline, ShowUsages, SendToAgent, CompareFileWith]);
actions!(ideae_edit, [DeleteLine, DuplicateLine, MoveLineUp, MoveLineDown, JoinLines, ToggleCase, NextOccurrence, GoToLine, RenameSymbol, FormatFile, Fold, Unfold, FoldAll, UnfoldAll, ParameterInfo, NextProblem, PrevProblem]);
actions!(ideae_task, [ToggleZoom, SendDiffToAgent, CommentOnDiff, FindInTerminal]);

actions!(ideae_task, [NextTab, PrevTab, NavigateBack, NavigateForward, CloseTab, ReopenClosedTab]);

/// Ctrl+1/2/3/`: the agent, Changes, History, a shell.
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(namespace = ideae_task, no_json)]
pub struct ShowView(pub usize);

/// Alt+1/2/3: the side panel's Changes, Files, Structure.
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(namespace = ideae_task, no_json)]
pub struct ShowSidePanel(pub usize);

pub struct FileView {
    pub(super) path: PathBuf,
    pub(super) editor: Entity<EditorState>,
    /// The text as last read from or written to disk, and that file's mtime.
    pub(super) saved: Print,
    pub(super) disk_mtime: Option<std::time::SystemTime>,
    pub dirty: bool,
    /// Changed on disk while it had unsaved edits: the user picks a side.
    pub(super) conflict: bool,
    /// The last save failed, with why.
    pub(super) error: Option<String>,
    pub(super) lsp_version: i32,
    /// Bumped on every change of the text, for caches of things derived from it.
    pub edit_version: u64,
    pub(super) _lsp_sync: Task<()>,
    /// The edit version being written, if a write is under way, and what
    /// orders it with a later one.
    pub(super) saving: Option<u64>,
    pub(super) save_seq: u64,
    pub(super) write_gate: Arc<WriteGate>,
    pub(super) _save: Task<()>,
    /// The task's worktree (for git) and the task, to open a blamed commit.
    pub(super) root: PathBuf,
    pub(super) task: WeakEntity<TaskView>,
    /// Who last changed each line, and the line the cursor is on.
    pub(super) blame: Rc<Vec<Option<Arc<git::BlameLine>>>>,
    pub(super) cursor_line: u32,
    /// The cursor's column, for the navigation history to come back to.
    pub(super) cursor_col: u32,
    pub(super) _blame: Task<()>,
    /// Blame shown by every line, in the gutter; made from `blame` once.
    pub(super) annotate: bool,
    pub(super) annotations: Option<Rc<gpui_kit::base::input::GutterNotes>>,
    /// The line whose annotation the pointer is on, and where it is.
    pub(super) blame_hover: Option<(usize, Point<Pixels>)>,
    /// Selections Ctrl+W grew from, for Ctrl+Shift+W to step back through.
    pub(super) expand_stack: Vec<std::ops::Range<usize>>,
    pub(super) last_expanded: Option<std::ops::Range<usize>>,
    /// What the language server is doing for this file, shown in the header.
    pub(super) lsp_status: SharedString,
    /// None while starting, then whether it works.
    pub(super) lsp_ok: Option<bool>,
    pub(super) lsp_client: Option<Arc<lsp::LspClient>>,
    pub(super) lsp_program: String,
    /// The server this file would have if it were installed.
    pub(super) lsp_missing: Option<&'static str>,
    /// Markdown shown as it reads instead of its source.
    pub(super) preview: bool,
    /// Git ignores it: while presenting it is not shown until asked for.
    pub(super) veiled: bool,
    /// The text whose next occurrence was added to the selection last,
    /// and where that one ends.
    pub(super) occurrence: Option<(String, usize)>,
    /// The file as the last commit has it, for the gutter's marks of what
    /// changed since; None for a file that commit lacks.
    pub(super) committed: Option<Arc<str>>,
    pub(super) _marks: Task<()>,
    /// The count of the language server's reports when its problems were
    /// last put into the editor.
    pub(super) problems_seen: u64,
    /// The signature shown above the cursor: its text, the parameter being
    /// written, the line it was asked on.
    pub(super) signature: Option<(String, Option<std::ops::Range<usize>>, u32)>,
    pub(super) _signature: Task<()>,
    pub(super) _poll: Task<()>,
}

/// How many 150 ms waits a task out of view lets pass before it refreshes.
pub(super) const HIDDEN_REFRESH_WAITS: usize = 20;

pub(super) fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A text as it was saved, without the text: its length and a hash. The
/// editor has the text already; a second copy of every open file is not
/// kept to tell whether it was edited.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Print {
    len: usize,
    hash: u64,
}

impl Print {
    fn add(hash: u64, bytes: &[u8]) -> u64 {
        // FNV-1a: the same whatever pieces the text comes in.
        bytes.iter().fold(hash, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
    }

    pub(super) fn of(text: &str) -> Self {
        Self { len: text.len(), hash: Self::add(0xcbf2_9ce4_8422_2325, text.as_bytes()) }
    }

    /// Whether the editor holds that text. Read through only when it is as
    /// long: while typing it seldom is.
    pub(super) fn is(&self, rope: &Rope) -> bool {
        rope.len() == self.len && rope.chunks().fold(0xcbf2_9ce4_8422_2325, |hash, chunk| Self::add(hash, chunk.as_bytes())) == self.hash
    }
}

/// Whether the editor holds exactly `text`, without copying it out.
pub(super) fn same_text(rope: &Rope, text: &str) -> bool {
    let mut rest = text.as_bytes();
    rope.len() == rest.len()
        && rope.chunks().all(|chunk| {
            let (head, tail) = rest.split_at(chunk.len());
            rest = tail;
            head == chunk.as_bytes()
        })
}

/// The file was written by another since it was last read: not written over.
#[derive(Debug, PartialEq)]
pub(super) struct ChangedOnDisk;

/// The last write made of a file, and the file's time after it.
pub(super) type WriteGate = std::sync::Mutex<(u64, Option<std::time::SystemTime>)>;

/// Writes of one file, in the order they were asked for: a write that
/// starts after a later one has been made is dropped. Gives the file's
/// time once written. A file that is not as `known`, nor as the last write
/// here left it, was changed by another (the agent, git) and no refresh
/// has seen it yet: it is left as it is.
pub(super) fn write_in_order(
    gate: &WriteGate,
    seq: u64,
    path: &std::path::Path,
    known: Option<std::time::SystemTime>,
    bytes: &[u8],
) -> std::io::Result<Result<Option<std::time::SystemTime>, ChangedOnDisk>> {
    let mut latest = gate.lock().unwrap();
    if seq > latest.0 {
        let now = mtime(path);
        if now.is_some() && now != known && now != latest.1 {
            return Ok(Err(ChangedOnDisk));
        }
        std::fs::write(path, bytes)?;
        *latest = (seq, mtime(path));
    }
    Ok(Ok(mtime(path)))
}

/// The task's file watcher and the directories it watches now: each on its
/// own, and those under git's refs with all that is in them.
pub(super) struct Watching {
    pub(super) watcher: Option<notify::RecommendedWatcher>,
    pub(super) watched: (Arc<BTreeSet<PathBuf>>, Vec<PathBuf>),
}

impl Watching {
    pub(super) fn sync(&mut self, watch: (Arc<BTreeSet<PathBuf>>, Vec<PathBuf>)) {
        if (Arc::ptr_eq(&watch.0, &self.watched.0) || watch.0 == self.watched.0) && watch.1 == self.watched.1 {
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
}

/// A file found changed on disk: the time the view knew, the time now,
/// and what is in it.
pub(super) struct DiskChange {
    pub(super) known: Option<std::time::SystemTime>,
    pub(super) now: std::time::SystemTime,
    pub(super) content: String,
}

/// Off the UI thread: of the open files, those whose time on disk is not
/// the one known, read.
pub(super) fn changed_on_disk(known: Vec<(PathBuf, Option<std::time::SystemTime>)>) -> Vec<Option<DiskChange>> {
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
    pub(super) fn mark_changes(&mut self, typing: bool, cx: &mut Context<Self>) {
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

    pub(super) fn on_edit(&mut self, cx: &mut Context<Self>) {
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
        let was_dirty = std::mem::replace(&mut self.dirty, !self.saved.is(self.editor.read(cx).text()));
        if self.annotate && was_dirty != self.dirty {
            self.show_annotations(cx);
        }
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
    pub(super) fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value();
        self.save_seq += 1;
        self.saving = Some(self.edit_version);
        let (seq, path, gate, known) = (self.save_seq, self.path.clone(), self.write_gate.clone(), self.disk_mtime);
        self._save = cx.spawn(async move |this, cx| {
            let bytes = text.clone();
            let written = cx.background_spawn(async move { write_in_order(&gate, seq, &path, known, bytes.as_bytes()) }).await;
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
        let written = write_in_order(&self.write_gate, self.save_seq, &self.path, self.disk_mtime, text.as_bytes());
        self.written(text, written, cx);
    }

    pub(super) fn written(&mut self, text: SharedString, written: std::io::Result<Result<Option<std::time::SystemTime>, ChangedOnDisk>>, cx: &mut Context<Self>) {
        match written {
            // The bar asks which to keep; the next refresh reads what is there.
            Ok(Err(ChangedOnDisk)) => self.conflict = true,
            Ok(Ok(mtime)) => {
                self.dirty = !same_text(self.editor.read(cx).text(), &text);
                self.saved = Print::of(&text);
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
    pub(super) fn disk_known(&self) -> Option<(PathBuf, Option<std::time::SystemTime>)> {
        self.saving.is_none().then(|| (self.path.clone(), self.disk_mtime))
    }

    /// Called on every worktree refresh with what `changed_on_disk` read:
    /// picks up changes made on disk (by the agent, git, another editor).
    /// Clean buffers reload in place; edited ones get a choice instead of
    /// losing either side.
    pub(super) fn apply_disk(&mut self, change: DiskChange, window: &mut Window, cx: &mut Context<Self>) {
        // Saved or reloaded since the disk was read: the next refresh looks again.
        if self.saving.is_some() || self.disk_mtime != change.known {
            return;
        }
        self.disk_mtime = Some(change.now);
        if Print::of(&change.content) == self.saved {
            // Touched and no more: nothing to choose between.
            if std::mem::take(&mut self.conflict) {
                cx.notify();
            }
            return;
        }
        if self.dirty {
            self.conflict = true;
            cx.notify();
        } else {
            self.reload(change.content, window, cx);
        }
    }

    pub(super) fn reload(&mut self, content: String, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_version += 1;
        self.saved = Print::of(&content);
        self.conflict = false;
        let content_for_lsp = content.clone();
        self.editor.update(cx, |state, cx| {
            let cursor = state.cursor_position();
            let bookmarks = state.bookmark_rows();
            state.set_value(content, window, cx);
            state.set_bookmarks(&bookmarks, super::bookmarks::bookmark_color(), cx);
            // Not `set_cursor_position`, which takes the keyboard: the file may
            // be reloading in a task out of view, as its agent writes it.
            state.place_cursor(cursor, cx);
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

    pub(super) fn reload_from_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                this.annotations = None;
                this.show_annotations(cx);
                this.committed = committed.map(Arc::from);
                this.mark_changes(false, cx);
                cx.notify();
            });
        });
    }

    pub(super) fn render_blame(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
    pub(super) fn expand_selection(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn shrink_selection(&mut self, cx: &mut Context<Self>) {
        let Some(previous) = self.expand_stack.pop() else { return };
        self.last_expanded = Some(previous.clone());
        self.editor.update(cx, |state, cx| state.set_selected_range(previous, cx));
    }

    /// Ctrl+/: comment the selected lines out, or back in when all are.
    pub(super) fn toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn edit(&mut self, window: &mut Window, cx: &mut Context<Self>, change: impl FnOnce(&str, std::ops::Range<usize>) -> Option<crate::edit::Edit>) {
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
    pub(super) fn select_next_occurrence(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn parameter_info(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.lsp_client.clone() else { return };
        let path = self.path.clone();
        self._signature = cx.spawn(async move |this, cx| {
            // After the keys of a burst, not each of them; and the text as it is then.
            cx.background_executor().timer(std::time::Duration::from_millis(80)).await;
            let Ok((text, pos)) = this.update(cx, |file, cx| {
                let state = file.editor.read(cx);
                (state.text().clone(), state.cursor_position())
            }) else {
                return;
            };
            let uri = lsp::path_to_uri(&path);
            let help = cx
                .background_spawn(crate::procs::on_thread(move || {
                    // Made one string where there is time for it.
                    client.did_change(&path, 0, &text.to_string())?;
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
    pub(super) fn render_signature(&self, cx: &App) -> Option<AnyElement> {
        let (label, active, line) = self.signature.as_ref()?;
        let state = self.editor.read(cx);
        if state.cursor_position().line != *line {
            return None;
        }
        let cursor = state.cursor();
        // The character before the cursor starts one to four bytes back.
        let text = state.text();
        let before = (1..=4).find_map(|back| cursor.checked_sub(back).filter(|at| text.char_at(*at).is_some())).unwrap_or(cursor);
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

    pub(super) fn fold(&mut self, fold: bool, all: bool, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            state.fold(fold, all, cx);
        });
    }

    /// The language server's latest word on what is wrong in the file, into
    /// the editor: underlined there, with the fixes it offers.
    pub(super) fn show_problems(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.lsp_client else { return };
        let Some((report, found)) = client.problems_since(&lsp::path_to_uri(&self.path), self.problems_seen) else { return };
        self.problems_seen = report;
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
    /// them up, written off the UI thread: a name can be in hundreds of
    /// them. `then` is told how many files changed.
    pub(super) fn apply_workspace_edit(
        &mut self,
        edit: &lsp_types::WorkspaceEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(usize, &mut Window, &mut Context<Self>) + 'static,
    ) {
        // Another file of it open with edits not saved: the server placed its
        // changes by what is on disk, and written there they would be at odds
        // with the tab. Nothing is applied, rather than a part of it.
        let by_file = lsp::edits_by_file(edit);
        let this = cx.entity_id();
        let unsaved = self.task.read_with(cx, |task, cx| task.unsaved_files(this, cx)).unwrap_or_default();
        if let Some(open) = by_file.iter().map(|(path, _)| path).find(|path| unsaved.contains(path)) {
            let name = open.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            window.push_notification(Notification::info(format!("Not applied: {name} has edits that are not saved. Save it and ask again.")).title("Language server"), cx);
            return;
        }
        let mut here = 0;
        let mut others = Vec::new();
        for (path, edits) in by_file {
            if path == self.path {
                self.edit(window, cx, |text, selection| {
                    let rewritten = crate::edit::rewrite(text, &lsp::apply_edits(text, &edits), selection.start);
                    here += usize::from(rewritten.is_some());
                    rewritten
                });
            } else {
                others.push((path, edits));
            }
        }
        if others.is_empty() {
            return then(here, window, cx);
        }
        cx.spawn_in(window, async move |this, cx| {
            let written = cx
                .background_spawn(async move {
                    others
                        .into_iter()
                        .filter(|(path, edits)| {
                            let Ok(text) = std::fs::read_to_string(path) else { return false };
                            let new = lsp::apply_edits(&text, edits);
                            new != text && std::fs::write(path, new).is_ok()
                        })
                        .count()
                })
                .await;
            let _ = this.update_in(cx, |_, window, cx| then(here + written, window, cx));
        })
        .detach();
    }

    /// The server is told the text as it is now, then asked: the answer
    /// comes back to `then`, in the window.
    pub(super) fn ask_server<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        ask: impl FnOnce(&lsp::LspClient, &str, lsp_types::Position) -> anyhow::Result<T> + Send + 'static,
        then: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.ask_server_or(window, cx, ask, then, None)
    }

    /// As `ask_server`; with `without`, that is done when there is no
    /// answer to apply, and the stale answer is not told of.
    fn ask_server_or<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        ask: impl FnOnce(&lsp::LspClient, &str, lsp_types::Position) -> anyhow::Result<T> + Send + 'static,
        then: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        without: Option<fn(&mut Self, &mut Context<Self>)>,
    ) {
        let Some(client) = self.lsp_client.clone() else {
            match without {
                Some(without) => without(self, cx),
                None => window.push_notification(Notification::info("No language server runs for this file."), cx),
            }
            return;
        };
        let (path, text, pos) = {
            let state = self.editor.read(cx);
            (self.path.clone(), state.text().clone(), state.cursor_position())
        };
        let asked_of = self.edit_version;
        cx.spawn_in(window, async move |this, cx| {
            let uri = lsp::path_to_uri(&path);
            let answer = cx
                .background_spawn(crate::procs::on_thread(move || {
                    client.did_change(&path, 0, &text.to_string())?;
                    ask(&client, &uri, pos)
                }))
                .await;
            let _ = this.update_in(cx, |this, window, cx| match answer {
                // The answer is about the text as it was: put onto the one
                // typed since, its changes would land in the wrong places.
                Ok(_) if this.edit_version != asked_of => match without {
                    Some(without) => without(this, cx),
                    None => window.push_notification(Notification::info("The text changed while the language server was asked: nothing was applied. Ask again.").title("Language server"), cx),
                },
                Ok(answer) => then(this, answer, window, cx),
                Err(e) => {
                    window.push_notification(Notification::error(format!("{e:#}")).title("Language server"), cx);
                    if let Some(without) = without {
                        without(this, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Formats the file as its language server would.
    pub(super) fn format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.format_then(false, window, cx)
    }

    /// Reformats, and with `save` writes the file once that is done.
    fn format_then(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Asked to save, the file is saved whatever comes of the formatting: as it is, when the server fails or the text moved on.
        let without: Option<fn(&mut Self, &mut Context<Self>)> = if save { Some(|this, cx| this.save(cx)) } else { None };
        let then = move |this: &mut Self, edits: Vec<lsp_types::TextEdit>, window: &mut Window, cx: &mut Context<Self>| {
            this.edit(window, cx, |text, selection| crate::edit::rewrite(text, &lsp::apply_edits(text, &edits), selection.start));
            if save {
                this.save(cx);
            }
        };
        self.ask_server_or(window, cx, |client, uri, _| client.formatting(uri, 4, true), then, without);
    }

    /// Asks for a new name of the symbol under the cursor and gives it that
    /// name everywhere the language server knows it.
    pub(super) fn rename_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                    let told = |files: usize, window: &mut Window, cx: &mut Context<Self>| {
                        let told = match files {
                            0 => "Nothing to rename here.".to_string(),
                            1 => "Renamed in 1 file.".to_string(),
                            n => format!("Renamed in {n} files."),
                        };
                        window.push_notification(Notification::info(told), cx);
                    };
                    match edit {
                        Some(edit) => this.apply_workspace_edit(&edit, window, cx, told),
                        None => told(0, window, cx),
                    }
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
    pub(super) fn go_to_line_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn go_to_line(&mut self, line: u32, col: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.jump(line, col, window, cx);
        self.editor.read(cx).focus_handle(cx).focus(window, cx);
    }

    /// F2 / Shift+F2: the next or previous problem the language server
    /// reported, errors before warnings, round the end of the file; what it
    /// says is shown there.
    pub(super) fn go_to_problem(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.editor.read(cx);
        let Some(set) = state.diagnostics() else { return };
        let at = state.cursor_position();
        let at = (at.line, at.character);
        let worst = set.iter().map(|d| problem_rank(d.severity)).max();
        let Some(worst) = worst.filter(|&r| r > 0) else { return };
        let starts = |d: &&gpui_kit::component::input::DiagnosticEntry| (d.diagnostic.range.start.line, d.diagnostic.range.start.character);
        let of_rank = || set.iter().filter(|d| problem_rank(d.severity) == worst);
        let picked = if forward {
            of_rank().find(|d| starts(d) > at).or_else(|| of_rank().next())
        } else {
            of_rank().filter(|d| starts(d) < at).last().or_else(|| of_rank().last())
        };
        let Some(entry) = picked.cloned() else { return };
        let start = entry.diagnostic.range.start;
        self.editor.update(cx, |state, cx| {
            state.jump_to_position(start, JUMP_AT, window, cx);
            state.present_diagnostic(entry, cx);
        });
        self.editor.read(cx).focus_handle(cx).focus(window, cx);
    }

    /// As `go_to_line`, the keyboard left where it is.
    pub(super) fn jump(&mut self, line: u32, col: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            state.jump_to_position(Position::new(line, col), JUMP_AT, window, cx);
        });
    }
}

impl FileView {
    pub(super) fn is_busy(&self) -> bool {
        self.lsp_client.as_ref().is_some_and(|c| c.busy().is_some())
    }

    pub(super) fn is_markdown(&self) -> bool {
        self.path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdx"))
    }

    /// The text as it is in the editor, unsaved edits too, drawn as
    /// Markdown. A link to another file of the worktree opens it here.
    pub(super) fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
    pub(super) fn lsp_problem(&self) -> Option<String> {
        self.lsp_client.as_ref().and_then(|c| c.problem())
    }

    pub(super) fn status_text(&self) -> String {
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
        let veil = self.veiled && crate::workspace::present::on() && crate::state::with_settings(|s| s.present_veil);
        let warning = hsla(0.08, 0.9, 0.55, 1.);
        v_flex()
            .size_full()
            .on_action(cx.listener(|this, _: &SaveFile, window, cx| {
                // Saved by hand, and asked to be tidied first: saved when the server has answered, or as it is when there is none.
                if crate::state::with_settings(|s| s.format_on_save) && this.lsp_client.is_some() {
                    return this.format_then(true, window, cx);
                }
                this.save(cx)
            }))
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
            .on_action(cx.listener(|this, _: &NextProblem, window, cx| this.go_to_problem(true, window, cx)))
            .on_action(cx.listener(|this, _: &PrevProblem, window, cx| this.go_to_problem(false, window, cx)))
            .on_action(cx.listener(|this, _: &FormatFile, window, cx| this.format(window, cx)))
            .on_action(cx.listener(|this, _: &ParameterInfo, _, cx| this.parameter_info(cx)))
            .on_action(cx.listener(|this, _: &Fold, _, cx| this.fold(true, false, cx)))
            .on_action(cx.listener(|this, _: &Unfold, _, cx| this.fold(false, false, cx)))
            .on_action(cx.listener(|this, _: &FoldAll, _, cx| this.fold(true, true, cx)))
            .on_action(cx.listener(|this, _: &UnfoldAll, _, cx| this.fold(false, true, cx)))
            .on_action(cx.listener(|this, _: &ToggleBlame, _, cx| this.toggle_blame(cx)))
            .on_action(cx.listener(|this, _: &ToggleBookmark, _, cx| this.toggle_bookmark(cx)))
            .on_action(cx.listener(|this, _: &NextBookmark, window, cx| this.go_to_bookmark(false, window, cx)))
            .on_action(cx.listener(|this, _: &PrevBookmark, window, cx| this.go_to_bookmark(true, window, cx)))
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
            .when(veil, |this| {
                this.child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .text_color(cx.theme().muted_foreground)
                        .child("Git ignores this file: it is not shown while presenting.")
                        .child(Button::new("unveil").small().outline().label("Show it anyway").on_click(cx.listener(|this, _, _, cx| {
                            this.veiled = false;
                            cx.notify();
                        }))),
                )
            })
            .when(self.preview && !veil, |this| this.child(self.render_preview(cx)))
            .when(!self.preview && !veil, |this| this.child(
                div()
                    .id("file-editor")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !hovered && this.blame_hover.take().is_some() {
                            cx.notify();
                        }
                    }))
                    .children(self.render_signature(cx))
                    .children(self.render_blame_hover(cx))
                    .child(crate::zoom::wheel(crate::zoom::Kind::Editor)).child(
                    Editor::new(&self.editor)
                        // Replaces the built-in menu, so its items are listed again.
                        .context_menu({
                            let (editor, annotating) = (self.editor.clone(), self.annotate);
                            move |menu, _, cx| {
                            // On the line numbers: what is about the lines.
                            if editor.read(cx).gutter_press().is_some() {
                                let blame = if annotating { "Close Annotations" } else { "Annotate with Git Blame" };
                                return menu.menu(blame, Box::new(ToggleBlame)).menu("Toggle Bookmark", Box::new(ToggleBookmark));
                            }
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
                                .menu("Compare with Branch, Tag or Commit…", Box::new(CompareFileWith))
                                .separator()
                                .menu("Send to Agent", Box::new(SendToAgent))
                        }})
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
pub(super) struct LspBridge {
    pub(super) client: Arc<lsp::LspClient>,
    pub(super) uri: String,
    pub(super) path: PathBuf,
    pub(super) file: WeakEntity<FileView>,
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
        // Off: only when asked for, which does not come this way.
        if !crate::state::with_settings(|s| s.complete_as_typed) {
            return false;
        }
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
        if !crate::state::with_settings(|s| s.semantic_colors) {
            return Task::ready(Ok(lsp_types::SemanticTokens::default()));
        }
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
                file.apply_workspace_edit(&edit, window, cx, |_, _, _| {});
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

/// Errors first, then warnings; hints and notes are no stops for F2.
fn problem_rank(severity: gpui_kit::component::input::DiagnosticSeverity) -> u8 {
    use gpui_kit::component::input::DiagnosticSeverity as S;
    match severity {
        S::Error => 2,
        S::Warning => 1,
        _ => 0,
    }
}
