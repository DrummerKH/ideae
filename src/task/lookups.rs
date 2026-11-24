use super::*;

impl TaskView {
    /// Which of the views `show_view` knows is in front: the agent, the
    /// changes, the history or a shell.
    /// Where in the task you are looking, for another variant to show the
    /// same: the file at the top of the diff (true), or the file open in
    /// the editor and the line the cursor is on.
    pub fn shown_place(&self, cx: &App) -> Option<(bool, String, u32)> {
        match &self.tabs.get(self.focused_ix()?)?.kind {
            TabKind::Changes => {
                let top = self.changes.read(cx).diff.read(cx).top_file()?;
                Some((true, top.to_string(), 0))
            }
            TabKind::File(file) => {
                let file = file.read(cx);
                let path = file.path.strip_prefix(&self.root).ok()?.to_string_lossy().into_owned();
                Some((false, path, file.editor.read(cx).cursor_position().line))
            }
            _ => None,
        }
    }

    /// The same place in this task: its changes at that file, or the file
    /// at that line. False when this task has no such file.
    pub fn show_place(&mut self, in_changes: bool, path: &str, line: u32, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if in_changes {
            match self.changed_paths.contains(path) {
                true => self.show_changed_file(SharedString::from(path.to_string()), window, cx),
                false => self.show_view(1, window, cx),
            }
            return true;
        }
        let file = self.root.join(path);
        let there = file.is_file();
        if there {
            self.open_file(file, line, window, cx);
        }
        there
    }

    pub fn shown_view(&self) -> usize {
        match self.focused_ix().and_then(|ix| self.tabs.get(ix)) {
            Some(tab) => match &tab.kind {
                TabKind::Changes => 1,
                TabKind::History => 2,
                TabKind::Terminal(_) if tab.agent.is_none() => 3,
                _ => 0,
            },
            None => 0,
        }
    }

    /// The file in the editor as an agent or a search takes it: its path from
    /// the project, and the line of the cursor, or those of the selection.
    pub(super) fn copy_reference(&mut self, cx: &mut Context<Self>) {
        let Some(TabKind::File(file)) = self.focused_ix().map(|ix| &self.tabs[ix].kind) else { return };
        let f = file.read(cx);
        let state = f.editor.read(cx);
        let range = state.selected_range();
        let from = state.text().offset_to_position(range.start).line + 1;
        // A selection ending at a line's start does not take that line.
        let end = state.text().offset_to_position(range.end);
        let to = if end.character == 0 && end.line + 1 > from { end.line } else { end.line + 1 };
        let relative = f.path.strip_prefix(&self.root).unwrap_or(&f.path).to_string_lossy().replace('\\', "/");
        let reference = if range.is_empty() || to <= from { format!("{relative}:{from}") } else { format!("{relative}:{from}-{to}") };
        cx.write_to_clipboard(ClipboardItem::new_string(reference));
    }

    pub(super) fn copy_file_path(&mut self, cx: &mut Context<Self>) {
        let Some(TabKind::File(file)) = self.focused_ix().map(|ix| &self.tabs[ix].kind) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(file.read(cx).path.display().to_string()));
    }

    pub(super) fn show_file_in_files(&mut self, cx: &mut Context<Self>) {
        let Some(TabKind::File(file)) = self.focused_ix().map(|ix| &self.tabs[ix].kind) else { return };
        let path = file.read(cx).path.clone();
        self.show_in_files(&path, cx);
    }

    pub(super) fn send_editor_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn show_outline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn show_usages_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn show_places(&mut self, path: PathBuf, pos: Position, implementations: bool, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn present_places(&mut self, word: &str, implementations: bool, places: Places, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn confirm_lsp_install(&mut self, program: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::notification::Notification;
        if crate::lsp_install::installed(program).is_some() {
            return self.reattach_lsp(program, cx);
        }
        let Some(recipe) = crate::lsp_install::recipe(program) else { return };
        if let Some(tool) = crate::lsp_install::missing_tool(&recipe) {
            let message = format!("Installing {program} needs {tool} on PATH.");
            return window.push_notification(Notification::warning(message).title(format!("Install {program}")), cx);
        }
        let description = format!("{}\n\nInto {}.", recipe.summary, crate::lsp_install::root().join(crate::lsp_install::home(program)).display());
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

    pub(super) fn install_lsp(&mut self, program: &'static str, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn notify_files(&self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let TabKind::File(file) = &tab.kind {
                file.update(cx, |_, cx| cx.notify());
            }
        }
    }

    /// Starts the newly installed server for the open files waiting for it,
    /// or for another installed with it (JSON's with CSS's).
    pub(super) fn reattach_lsp(&mut self, program: &'static str, cx: &mut Context<Self>) {
        use crate::lsp_install::home;
        let waiting: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| match &tab.kind {
                TabKind::File(file) if file.read(cx).lsp_missing.is_some_and(|p| home(p) == home(program)) => Some(file.clone()),
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
}
