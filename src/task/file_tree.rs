//! The Files panel: the worktree as a tree, each name in the color of what
//! git says of it, as JetBrains does. What `.gitignore` keeps out is there
//! too: git names an ignored folder once (`target/`), and what is in it is
//! read from disk when it is opened.

use super::*;
use gpui_kit::component::menu::PopupMenu;

/// What the color of a name in the tree tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mark {
    Modified,
    /// New, and added to git.
    Added,
    /// New, and not added to git.
    Untracked,
    Conflict,
    Ignored,
    /// Gone from disk: the Changes list shows it, the tree leaves it out.
    Deleted,
}

impl Mark {
    fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        let dark = theme.mode.is_dark();
        // On a light background the themes' blue and green are pale for text.
        let deepen = |color: Hsla| if dark { color } else { hsla(color.h, color.s, color.l * 0.8, color.a) };
        match self {
            Mark::Modified => deepen(theme.blue),
            Mark::Added => deepen(theme.green),
            Mark::Conflict => theme.red,
            // Brown-red as JetBrains' "unversioned", apart from a conflict's red.
            Mark::Untracked => {
                let red = theme.red;
                hsla((red.h + 0.04).fract(), red.s * 0.8, if dark { red.l } else { red.l * 0.8 }, 1.)
            }
            // Olive, dimmer than any name worked on.
            Mark::Ignored => hsla(theme.yellow.h, theme.yellow.s * 0.55, if dark { 0.55 } else { 0.4 }, 1.),
            Mark::Deleted => theme.muted_foreground,
        }
    }
}

/// A file's name in the color of its mark; unmarked, with its extension dimmed.
/// `dim`: seen already, as a viewed change.
pub(super) fn marked_label(name: SharedString, mark: Option<Mark>, dim: bool, cx: &App) -> StyledText {
    match mark {
        Some(mark) => {
            let color = mark.color(cx);
            let color = if dim { color.opacity(0.6) } else { color };
            let all = 0..name.len();
            StyledText::new(name).with_highlights([(all, HighlightStyle { color: Some(color), ..Default::default() })])
        }
        None => file_label(name, cx),
    }
}

impl From<git::Change> for Mark {
    fn from(change: git::Change) -> Self {
        match change {
            git::Change::Modified => Mark::Modified,
            git::Change::Added => Mark::Added,
            git::Change::Untracked => Mark::Untracked,
            git::Change::Conflict => Mark::Conflict,
            git::Change::Deleted => Mark::Deleted,
        }
    }
}

/// The marks of the changed files and of the folders they are in, by tree
/// id (a folder's ends in `/`). A folder with only new files in it is new
/// itself; one that holds a file git had before is modified. `files` is
/// sorted: a folder's files come together.
pub(super) fn tree_marks(states: &HashMap<String, git::FileState>, files: &[String]) -> HashMap<String, Mark> {
    let mut marks: HashMap<String, Mark> = HashMap::with_capacity(states.len());
    let mut folders: HashSet<&str> = HashSet::new();
    for (path, state) in states {
        marks.insert(path.clone(), state.change.into());
        folders.extend(path.match_indices('/').map(|(slash, _)| &path[..=slash]));
    }
    for folder in folders {
        let start = files.partition_point(|file| file.as_str() < folder);
        let mut mark = Mark::Untracked;
        for file in files[start..].iter().take_while(|file| file.starts_with(folder)) {
            match states.get(file).map(|state| state.change) {
                Some(git::Change::Untracked) => {}
                Some(git::Change::Added) => mark = Mark::Added,
                _ => {
                    mark = Mark::Modified;
                    break;
                }
            }
        }
        marks.insert(folder.to_string(), mark);
    }
    marks
}

/// What is in an ignored folder, read from disk: names, and whether each
/// is a folder; folders first.
pub(super) type Listing = Rc<Vec<(SharedString, bool)>>;

/// The most entries of an ignored folder the tree shows.
const LISTED: usize = 5000;

/// Id of the row standing in for what is not read yet, or not shown.
fn filler(folder: &str, what: &str) -> TreeItem {
    TreeItem::new(format!("{folder}\0"), what.to_string())
}

/// Nests sorted relative paths into tree items. Folder ids end in `/`, and
/// the folders in `expanded` stay open across rebuilds. The ignored folders
/// show what `listed` has of them; those open and not read yet are returned
/// to be read. `made`: folders git does not list, for having nothing in them.
pub(super) fn build_tree(
    files: &[String],
    gone: &HashSet<String>,
    ignored: &[String],
    made: &[String],
    listed: &HashMap<SharedString, Listing>,
    expanded: &HashSet<SharedString>,
) -> (Vec<TreeItem>, Vec<SharedString>) {
    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        files: Vec<String>,
        /// Ignored: what is in it comes from disk.
        ignored: bool,
    }
    let mut root = Node::default();
    let mut add = |path: &String| {
        let mut node = &mut root;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            match parts.peek() {
                // `target/`: the folder itself.
                Some(&"") => {
                    node.dirs.entry(part.to_string()).or_default().ignored = true;
                    break;
                }
                Some(_) => node = node.dirs.entry(part.to_string()).or_default(),
                None => node.files.push(path.clone()),
            }
        }
    };
    // Deleted and not staged so, git lists it still: not on disk, not in the tree.
    files.iter().filter(|file| !gone.contains(*file)).chain(ignored).for_each(&mut add);
    for folder in made {
        let mut node = &mut root;
        for part in folder.split('/').filter(|part| !part.is_empty()) {
            node = node.dirs.entry(part.to_string()).or_default();
        }
    }

    fn from_disk(id: &SharedString, listed: &HashMap<SharedString, Listing>, expanded: &HashSet<SharedString>, unread: &mut Vec<SharedString>) -> Vec<TreeItem> {
        let Some(entries) = listed.get(id) else {
            if expanded.contains(id) {
                unread.push(id.clone());
            }
            return vec![filler(id, "…")];
        };
        let mut out: Vec<TreeItem> = entries
            .iter()
            .take(LISTED)
            .map(|(name, folder)| {
                if *folder {
                    let child = SharedString::from(format!("{id}{name}/"));
                    let open = expanded.contains(&child);
                    TreeItem::new(child.clone(), name.clone()).children(from_disk(&child, listed, expanded, unread)).expanded(open)
                } else {
                    TreeItem::new(format!("{id}{name}"), name.clone())
                }
            })
            .collect();
        if entries.len() > LISTED {
            out.push(filler(id, &format!("{} more", entries.len() - LISTED)));
        }
        out
    }

    fn items(node: &Node, prefix: &str, listed: &HashMap<SharedString, Listing>, expanded: &HashSet<SharedString>, unread: &mut Vec<SharedString>) -> Vec<TreeItem> {
        let mut out = Vec::new();
        for (name, child) in &node.dirs {
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let id = SharedString::from(format!("{path}/"));
            let open = expanded.contains(&id);
            let children = if child.ignored { from_disk(&id, listed, expanded, unread) } else { items(child, &path, listed, expanded, unread) };
            out.push(TreeItem::new(id, name.clone()).children(children).expanded(open));
        }
        // Ignored files come after the others: sorted again with them.
        let mut files: Vec<&String> = node.files.iter().collect();
        if !files.is_sorted() {
            files.sort();
        }
        for file in files {
            let name = file.rsplit('/').next().unwrap_or(file);
            out.push(TreeItem::new(file.clone(), name.to_string()));
        }
        out
    }
    let mut unread = Vec::new();
    let items = items(&root, "", listed, expanded, &mut unread);
    (items, unread)
}

/// What is in `folder`, folders first, each by name.
fn list(folder: &std::path::Path) -> Vec<(SharedString, bool)> {
    let mut entries: Vec<(SharedString, bool)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.file_type().is_ok_and(|t| t.is_dir())))
        .filter(|(name, _)| !git::is_junk(name))
        .map(|(name, folder)| (name.into(), folder))
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries
}

impl TaskView {
    /// Whether the tree row `id` is an ignored folder or in one.
    pub(super) fn in_ignored(&self, id: &str) -> bool {
        self.tree_ignored.iter().any(|ignored| ignored.ends_with('/') && id.starts_with(ignored.as_str()))
    }

    pub(super) fn apply_ignored(&mut self, ignored: Vec<String>, cx: &mut Context<Self>) {
        if *self.tree_ignored == ignored {
            return;
        }
        self.tree_ignored = Rc::new(ignored);
        let ignored = self.tree_ignored.clone();
        self.ignored_listed.retain(|id, _| ignored.iter().any(|folder| folder.ends_with('/') && id.starts_with(folder.as_str())));
        self.tree_stale = true;
        self.fill_tree(cx);
        cx.notify();
    }

    /// Whether the files gone from disk are others: the tree is to be built again.
    pub(super) fn mark_tree(&mut self) -> bool {
        self.tree_marks = Rc::new(tree_marks(&self.file_states, &self.tree_files));
        let gone: HashSet<String> = self.file_states.iter().filter(|(_, state)| state.change == git::Change::Deleted).map(|(path, _)| path.clone()).collect();
        if *self.tree_gone == gone {
            return false;
        }
        self.tree_gone = Rc::new(gone);
        true
    }

    /// While presenting with ignored files hidden, the tree leaves them out:
    /// their names (`.env.local`) tell enough.
    pub(super) fn hide_ignored_in_tree(&mut self, cx: &mut Context<Self>) {
        let hide = crate::workspace::present::on() && crate::state::with_settings(|s| s.present_veil);
        if hide != self.tree_hides_ignored {
            self.tree_hides_ignored = hide;
            self.ignored_listed.clear();
            self.tree_stale = true;
            self.fill_tree(cx);
            cx.notify();
        }
    }

    pub(super) fn tree_expanded(&mut self, id: &SharedString, cx: &mut Context<Self>) {
        self.expanded.insert(id.clone());
        // Read again on every opening: a build writes there all the time.
        if self.in_ignored(id) {
            self.list_ignored(id.clone(), cx);
        }
    }

    pub(super) fn tree_collapsed(&mut self, id: &SharedString) {
        self.expanded.remove(id);
        // Closed, what it showed is let go of: read again when opened.
        self.ignored_listed.retain(|listed, _| !listed.starts_with(id.as_ref()));
    }

    /// Reads an ignored folder off the UI thread, and shows it.
    pub(super) fn list_ignored(&mut self, id: SharedString, cx: &mut Context<Self>) {
        if !self.ignored_reading.insert(id.clone()) {
            return;
        }
        let folder = self.root.join(id.as_ref());
        cx.spawn(async move |this, cx| {
            let entries = cx.background_spawn(async move { list(&folder) }).await;
            let _ = this.update(cx, |this, cx| {
                this.ignored_reading.remove(&id);
                // Closed or no longer ignored meanwhile: not wanted any more.
                if !this.expanded.contains(&id) || !this.in_ignored(&id) {
                    return;
                }
                if this.ignored_listed.get(&id).is_some_and(|seen| **seen == entries) {
                    return;
                }
                this.ignored_listed.insert(id, Rc::new(entries));
                this.tree_stale = true;
                this.fill_tree(cx);
            });
        })
        .detach();
    }

    /// The Files panel's tree, built when the panel is the one shown: it
    /// holds every path again, several times over, in every open task.
    pub(super) fn fill_tree(&mut self, cx: &mut Context<Self>) {
        let shown = self.visible && self.panel_shown(RightPane::Files);
        if self.tree_stale && shown {
            self.tree_stale = false;
            let ignored: &[String] = if self.tree_hides_ignored { &[] } else { &self.tree_ignored };
            let (items, unread) = build_tree(&self.tree_files, &self.tree_gone, ignored, &self.made_folders, &self.ignored_listed, &self.expanded);
            self.tree.update(cx, |t, cx| t.set_items(items, cx));
            for id in unread {
                self.list_ignored(id, cx);
            }
        } else if !self.tree_stale && !shown {
            // Let go of when not shown: every path again, in every task that showed it once.
            self.tree_stale = true;
            self.ignored_listed.clear();
            self.tree.update(cx, |t, cx| t.set_items(Vec::new(), cx));
        }
    }

    pub(super) fn render_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        let root = self.root.clone();
        let marks = self.tree_marks.clone();
        let ignored = if self.tree_hides_ignored { Rc::default() } else { self.tree_ignored.clone() };
        let touched = self.agent_touched.clone();
        let editing = self.agent_editing.clone();
        let (menu_view, menu_root) = (view.clone(), root.clone());
        div().flex_1().min_h_0().child(
            tree(&self.tree, move |ix, entry, selected, _, cx| {
                let item = entry.item();
                let row = ListItem::new(ix).w_full().py_0p5().px_2().pl(px(16.) * entry.depth() + px(4.)).selected(selected);
                // "…" while a folder is read, or what more it has than is shown.
                if item.id.ends_with('\0') {
                    return row.child(h_flex().text_sm().pl(px(40.)).text_color(cx.theme().muted_foreground).child(item.label.clone()));
                }
                // An ignored folder read empty has no rows under it: a folder still.
                let folder = item.id.ends_with('/');
                let mark = marks.get(item.id.as_ref()).copied().or_else(|| {
                    ignored.iter().any(|ignored| item.id.starts_with(ignored.as_str()) && (ignored.ends_with('/') || ignored.len() == item.id.len())).then_some(Mark::Ignored)
                });
                let label = if folder && mark.is_none() { StyledText::new(item.label.clone()) } else { marked_label(item.label.clone(), mark, false, cx) };
                // Folders have one icon, open or not; the chevron before it tells.
                let chevron = div().size_4().flex_none().flex().items_center().justify_center().when(entry.is_folder(), |this| {
                    let chevron = if entry.is_expanded() { IconName::ChevronDown } else { IconName::ChevronRight };
                    this.child(gpui_kit::component::Icon::new(chevron).size_3p5().text_color(cx.theme().muted_foreground))
                });
                let icon = crate::file_icons::icon(if folder { crate::file_icons::folder(&item.label) } else { crate::file_icons::file_kept(&item.label) }, cx);
                // The agent's work; a folder leads to the file being written.
                let on_the_way = editing.as_deref().is_some_and(|e| e.strip_prefix(item.id.as_ref()).is_some_and(|rest| rest.is_empty() || (folder && !rest.is_empty())));
                let agent = if on_the_way && !folder {
                    Some("editing")
                } else if on_the_way || touched.contains(&item.id) {
                    Some("●")
                } else {
                    None
                };
                let path = root.join(item.id.as_ref());
                let dragged = DraggedFiles::new(vec![path.clone()], !folder, root.clone());
                let view = view.clone();
                row.child(
                    h_flex()
                        .gap_1p5()
                        .text_sm()
                        .child(chevron)
                        .child(icon)
                        .child(div().truncate().child(label))
                        .when_some(agent, |this, mark| this.child(div().text_xs().text_color(gpui_kit::blue()).child(mark))),
                )
                .on_drag(dragged, |dragged, _, _, cx| {
                    start_drag(Drag::Files);
                    cx.new(|_| dragged.clone())
                })
                .on_click(move |event, window, cx| {
                    if folder {
                        return;
                    }
                    let path = path.clone();
                    let how = if event.click_count() > 1 { OpenAs::Kept } else { OpenAs::Preview };
                    let _ = view.update(cx, |t, cx| t.open_file_as(path, 0, 0, how, window, cx));
                })
            })
            .context_menu(move |_, entry, menu, _, _| {
                let id = entry.item().id.clone();
                if id.ends_with('\0') {
                    return menu;
                }
                TaskView::tree_menu(menu_view.clone(), menu_root.clone(), id, menu)
            })
            .size_full(),
        )
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if event.keystroke.key == "f2" && !event.keystroke.modifiers.modified() {
                cx.stop_propagation();
                this.rename_selected(window, cx);
            }
        }))
    }
}

/// The folder of a tree row, relative and without the slash: the folder
/// itself, or the one the file is in; "" at the top.
fn folder_of(id: &str) -> &str {
    match id.strip_suffix('/') {
        Some(folder) => folder,
        None => id.rsplit_once('/').map_or("", |(folder, _)| folder),
    }
}

/// Whether `name` names a file or folder, or for a new one a path under
/// where it goes (`with_folders`): nothing that leads out of it.
fn name_ok(name: &str, with_folders: bool) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Give it a name");
    }
    if name.contains('\\') || (!with_folders && name.contains('/')) {
        return Err("A name has no slash in it");
    }
    if name.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
        return Err("Not a name a file can have here");
    }
    Ok(())
}

/// `old` moved to `new`, and `path` was it or in it: where it is now.
fn moved(path: &std::path::Path, old: &std::path::Path, new: &std::path::Path) -> Option<PathBuf> {
    path.strip_prefix(old).ok().map(|rest| if rest.as_os_str().is_empty() { new.to_path_buf() } else { new.join(rest) })
}

/// What a name dialog does with the name, off the UI thread: the path it
/// made, or why not.
type NameJob = Arc<dyn Fn(&str) -> Result<PathBuf, String> + Send + Sync>;
/// What is done after, on the UI thread, with that path.
type MadeIt = Rc<dyn Fn(&mut TaskView, PathBuf, &mut Window, &mut Context<TaskView>)>;

/// Shows `path` in the system's file manager: the folder, or the folder
/// of the file (on Windows and macOS with the file picked).
pub(super) fn reveal(path: PathBuf, folder: bool, cx: &mut App) {
    cx.background_spawn(crate::procs::on_thread(move || {
        #[cfg(target_os = "macos")]
        let mut command = {
            let _ = folder;
            let mut command = crate::procs::command("open");
            command.arg("-R").arg(&path);
            command
        };
        #[cfg(windows)]
        let mut command = {
            let _ = folder;
            let mut command = crate::procs::command("explorer");
            command.arg(format!("/select,{}", path.display()));
            command
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = {
            let mut command = crate::procs::command("xdg-open");
            command.arg(if folder { path.as_path() } else { path.parent().unwrap_or(&path) });
            command
        };
        let _ = command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }))
    .detach();
}

impl TaskView {
    /// The Files panel's menu of a row: `id` as the tree has it.
    pub(super) fn tree_menu(view: WeakEntity<Self>, root: PathBuf, id: SharedString, menu: PopupMenu) -> PopupMenu {
        use gpui_kit::component::menu::PopupMenuItem;
        let folder = id.ends_with('/');
        let relative = id.trim_end_matches('/').to_string();
        let absolute = root.join(&relative);
        let at = |run: fn(&mut TaskView, SharedString, &mut Window, &mut Context<TaskView>)| {
            let (view, id) = (view.clone(), id.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |task, cx| run(task, id.clone(), window, cx));
            }
        };
        let (shown, copied, copied_relative) = (absolute.clone(), absolute.to_string_lossy().into_owned(), relative.clone());
        let here = if folder { absolute.clone() } else { absolute.parent().map_or(root.clone(), PathBuf::from) };
        let terminal_view = view.clone();
        menu.item(PopupMenuItem::new("New file…").on_click(at(|task, id, window, cx| task.new_entry_dialog(folder_of(&id).to_string(), false, window, cx))))
            .item(PopupMenuItem::new("New folder…").on_click(at(|task, id, window, cx| task.new_entry_dialog(folder_of(&id).to_string(), true, window, cx))))
            .separator()
            .item(PopupMenuItem::new("Rename…").on_click(at(|task, id, window, cx| task.rename_entry_dialog(id, window, cx))))
            .item(PopupMenuItem::new("Delete").on_click(at(|task, id, window, cx| task.delete_entry(id, window, cx))))
            .separator()
            .item(PopupMenuItem::new("Copy path").on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()))))
            .item(PopupMenuItem::new("Copy relative path").on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copied_relative.clone()))))
            // The tree's menu takes no submenu: the ways into .gitignore are listed here.
            .separator()
            .map(|menu| {
                git::ignore_patterns(&relative, folder).into_iter().fold(menu, |menu, (label, pattern, _)| {
                    let view = view.clone();
                    menu.item(PopupMenuItem::new(format!("Add to .gitignore: {label}")).on_click(move |_, window, cx| {
                        let pattern = pattern.clone();
                        let _ = view.update(cx, |t, cx| t.run_action("Ignore", move |root| git::ignore(&root, &pattern, git::IgnoreIn::Shared), false, window, cx));
                    }))
                })
            })
            .separator()
            .item(PopupMenuItem::new("Reveal in file manager").on_click(move |_, _, cx| reveal(shown.clone(), folder, cx)))
            .item(PopupMenuItem::new("Open terminal here").on_click(move |_, window, cx| {
                let here = here.clone();
                let _ = terminal_view.update(cx, |task, cx| task.new_terminal_in(here, window, cx));
            }))
            .when(!folder, |menu| {
                menu.item(PopupMenuItem::new("Compare with…").on_click(at(|task, id, window, cx| task.compare_with_dialog(Some(id.to_string()), window, cx))))
            })
    }

    /// F2 in the tree: the row it is on.
    pub(super) fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.tree.read(cx).selected_item().map(|item| item.id.clone()) else { return };
        if !id.ends_with('\0') {
            self.rename_entry_dialog(id, window, cx);
        }
    }

    /// Asks for a name, and runs `job` with it off the UI thread; done, the
    /// dialog closes and `made` gets the path, else it tells why.
    #[allow(clippy::too_many_arguments)]
    fn name_dialog(&mut self, title: String, value: String, hint: String, ok: &'static str, with_folders: bool, job: NameJob, made: MadeIt, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| gpui_kit::component::input::InputState::new(window, cx));
        // The name without its extension, ready to be typed over.
        let stem = value.rfind('.').filter(|&dot| dot > 0).unwrap_or(value.len());
        input.update(cx, |i, cx| {
            i.set_value(value.clone(), window, cx);
            i.set_selected_range(0..stem, cx);
        });
        let said: Rc<std::cell::RefCell<Option<String>>> = Rc::default();
        let apply = {
            let (input, said) = (input.clone(), said.clone());
            Rc::new(move |_: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                let name = input.read(cx).value().trim().to_string();
                if let Err(why) = name_ok(&name, with_folders) {
                    *said.borrow_mut() = Some(why.to_string());
                    return window.refresh();
                }
                let (job, made, said) = (job.clone(), made.clone(), said.clone());
                cx.spawn_in(window, async move |this, cx| {
                    let result = cx.background_spawn(async move { job(&name) }).await;
                    let _ = this.update_in(cx, |this, window, cx| match result {
                        Ok(path) => {
                            window.close_dialog(cx);
                            made(this, path, window, cx);
                        }
                        Err(why) => {
                            *said.borrow_mut() = Some(why);
                            window.refresh();
                        }
                    });
                })
                .detach();
            })
        };
        let this = cx.weak_entity();
        let focused = Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, focused, this, apply, said, hint) = (input.clone(), focused.clone(), this.clone(), apply.clone(), said.clone(), hint.clone());
            dialog
                .title(title.clone())
                .w(px(460.))
                // Enter in the input reaches the dialog.
                .on_ok({
                    let (this, apply) = (this.clone(), apply.clone());
                    move |_, window, cx| {
                        let _ = this.update(cx, |task, cx| apply(task, window, cx));
                        false
                    }
                })
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let input = input.clone();
                        window.defer(cx, move |window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
                    }
                    content.child(
                        v_flex()
                            .gap_1()
                            .child(gpui_kit::component::input::Input::new(&input))
                            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(hint.clone()))
                            .children(said.borrow().clone().map(|why| div().text_xs().text_color(gpui_kit::red()).child(why))),
                    )
                })
                .footer(h_flex().justify_end().child(Button::new("name-ok").primary().label(ok).on_click(move |_, window, cx| {
                    let apply = apply.clone();
                    let _ = this.update(cx, |task, cx| apply(task, window, cx));
                })))
        });
    }

    /// New file… or New folder… in `folder` (relative, "" at the top).
    pub(super) fn new_entry_dialog(&mut self, folder: String, dir: bool, window: &mut Window, cx: &mut Context<Self>) {
        let base = self.root.join(&folder);
        let shown = if folder.is_empty() { "the top folder".to_string() } else { format!("{folder}/") };
        let job: NameJob = Arc::new(move |name| {
            let path = base.join(name);
            if path.symlink_metadata().is_ok() {
                return Err(format!("{name} is there already"));
            }
            if dir {
                std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            } else {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| e.to_string())?;
            }
            Ok(path)
        });
        let made: MadeIt = Rc::new(move |task, path, window, cx| task.made_entry(path, dir, window, cx));
        let (title, ok) = if dir { ("New folder", "Create") } else { ("New file", "Create") };
        self.name_dialog(title.into(), String::new(), format!("In {shown}; a name with / makes the folders on the way"), ok, true, job, made, window, cx);
    }

    /// A file or folder made from the tree: shown there, a file opened.
    fn made_entry(&mut self, path: PathBuf, dir: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(relative) = path.strip_prefix(&self.root).map(|p| p.to_string_lossy().replace('\\', "/")) else { return };
        // Every folder on the way is opened, so that it is seen.
        for (slash, _) in relative.match_indices('/') {
            self.expanded.insert(format!("{}/", &relative[..slash]).into());
        }
        if dir {
            self.expanded.insert(format!("{relative}/").into());
            let mut made = (*self.made_folders).clone();
            made.push(relative.clone());
            self.made_folders = Rc::new(made);
        }
        let parent = format!("{}/", folder_of(&relative));
        if self.in_ignored(&parent) {
            self.list_ignored(parent.into(), cx);
        }
        self.tree_stale = true;
        self.fill_tree(cx);
        let _ = self.refresh_tx.try_send(());
        if !dir {
            self.open_file_as(path, 0, 0, OpenAs::Kept, window, cx);
        }
        cx.notify();
    }

    pub(super) fn rename_entry_dialog(&mut self, id: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let relative = id.trim_end_matches('/').to_string();
        let from = self.root.join(&relative);
        let name = relative.rsplit('/').next().unwrap_or(&relative).to_string();
        let job_from = from.clone();
        let job: NameJob = Arc::new(move |name| {
            let to = job_from.with_file_name(name);
            if to == job_from {
                return Ok(to);
            }
            // Another file of that name, not this one with its case changed.
            let same = |a: &std::path::Path, b: &std::path::Path| std::fs::canonicalize(a).ok().is_some_and(|a| std::fs::canonicalize(b).ok() == Some(a));
            if to.symlink_metadata().is_ok() && !same(&job_from, &to) {
                return Err(format!("{name} is there already"));
            }
            std::fs::rename(&job_from, &to).map_err(|e| e.to_string())?;
            Ok(to)
        });
        let made: MadeIt = Rc::new(move |task, to, window, cx| task.renamed(from.clone(), to, window, cx));
        let hint = if id.ends_with('/') { "The tabs open on what is in it follow it, with what is not saved" } else { "A tab open on it follows it, with what is not saved" };
        self.name_dialog(format!("Rename {name}"), name, hint.to_string(), "Rename", false, job, made, window, cx);
    }

    /// After a rename: open tabs of it, or in it, follow, as does the tree.
    fn renamed(&mut self, from: PathBuf, to: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let title = |path: &std::path::Path| SharedString::from(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        for ix in 0..self.tabs.len() {
            match &self.tabs[ix].kind {
                TabKind::File(file) => {
                    let Some(new) = moved(&file.read(cx).path, &from, &to) else { continue };
                    let file = file.clone();
                    let (old, client, editor) = {
                        let f = file.read(cx);
                        (f.path.clone(), f.lsp_client.clone(), f.editor.clone())
                    };
                    // The server knows the text by its old name: told it is closed, and given it under the new one.
                    if let Some(client) = client {
                        cx.background_spawn(crate::procs::on_thread(move || {
                            let _ = client.did_close(&old);
                        }))
                        .detach();
                    }
                    file.update(cx, |f, cx| {
                        f.path = new.clone();
                        cx.notify();
                    });
                    self.tabs[ix].title = title(&new);
                    let content = editor.read(cx).value().to_string();
                    self.attach_lsp(&new, content, &editor, &file, cx);
                }
                TabKind::Binary(binary) => {
                    let Some(new) = moved(&binary.read(cx).path, &from, &to) else { continue };
                    binary.update(cx, |b, _| b.path = new.clone());
                    self.tabs[ix].title = title(&new);
                }
                _ => {}
            }
        }
        let (Ok(old), Ok(new)) = (from.strip_prefix(&self.root), to.strip_prefix(&self.root)) else { return };
        let (old, new) = (old.to_string_lossy().replace('\\', "/"), new.to_string_lossy().replace('\\', "/"));
        let follow = |path: &str| -> Option<String> {
            if path == old {
                Some(new.clone())
            } else {
                path.strip_prefix(&format!("{old}/")).map(|rest| format!("{new}/{rest}"))
            }
        };
        self.expanded = self.expanded.iter().map(|id| follow(id.trim_end_matches('/')).map_or(id.clone(), |moved| format!("{moved}/").into())).collect();
        self.made_folders = Rc::new(self.made_folders.iter().map(|folder| follow(folder).unwrap_or_else(|| folder.clone())).collect());
        self.tree_stale = true;
        self.fill_tree(cx);
        let _ = self.refresh_tx.try_send(());
        let _ = window;
        cx.notify();
    }

    /// Delete: to the trash, with Undo; a folder is asked about first. With
    /// no trash to go to, asked, and deleted for good.
    pub(super) fn delete_entry(&mut self, id: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let folder = id.ends_with('/');
        let relative = id.trim_end_matches('/').to_string();
        let path = self.root.join(&relative);
        if cfg!(windows) {
            return self.delete_for_good_dialog(path, folder, None, window, cx);
        }
        if !folder {
            return self.trash_entry(path, folder, window, cx);
        }
        let this = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, path) = (this.clone(), path.clone());
            dialog
                .confirm()
                .title(format!("Move {relative}/ to the trash?"))
                .description("The folder and all that is in it. Undo brings it back.")
                .ok_text("Move to trash")
                .on_ok(move |_, window, cx| {
                    let path = path.clone();
                    let _ = this.update(cx, |task, cx| task.trash_entry(path, true, window, cx));
                    true
                })
        });
    }

    fn trash_entry(&mut self, path: PathBuf, folder: bool, window: &mut Window, cx: &mut Context<Self>) {
        // What is open of it is saved first, so that Undo brings back the last of it.
        self.save_all_now(cx);
        let job_path = path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(crate::procs::on_thread(move || crate::trash::trash(&job_path))).await;
            let _ = this.update_in(cx, |task, window, cx| {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                match result {
                    Ok(trashed) => {
                        task.deleted(&path, window, cx);
                        let view = cx.weak_entity();
                        let mut note = Notification::info(format!("Moved {name} to the trash"));
                        if let Some(trashed) = trashed {
                            note = note
                                .action(move |_, _, cx| {
                                    let (view, trashed, me) = (view.clone(), trashed.clone(), cx.entity().downgrade());
                                    Button::new("undo-trash").xsmall().outline().label("Undo").on_click(move |_, window, cx| {
                                        let trashed = trashed.clone();
                                        let _ = view.update(cx, |task, cx| task.untrash(trashed, window, cx));
                                        let _ = me.update(cx, |note, cx| note.dismiss(window, cx));
                                    })
                                })
                                .autohide(true);
                        }
                        window.push_notification(note, cx);
                    }
                    Err(why) => task.delete_for_good_dialog(path, folder, Some(why), window, cx),
                }
            });
        })
        .detach();
    }

    fn untrash(&mut self, trashed: crate::trash::Trashed, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { crate::trash::restore(&trashed) }).await;
            let _ = this.update_in(cx, |task, window, cx| {
                if let Err(why) = result {
                    window.push_notification(Notification::error(format!("Not brought back: {why}")), cx);
                }
                let _ = task.refresh_tx.try_send(());
            });
        })
        .detach();
    }

    fn delete_for_good_dialog(&mut self, path: PathBuf, folder: bool, why: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let name = path.strip_prefix(&self.root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let shown = if folder { format!("{name}/") } else { name };
        let description = match why {
            Some(why) => format!("It cannot go to the trash ({why}). Deleted, it is gone for good."),
            None if folder => "The folder and all that is in it are gone for good.".to_string(),
            None => "It is gone for good.".to_string(),
        };
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, path) = (this.clone(), path.clone());
            dialog
                .confirm()
                .title(format!("Delete {shown}?"))
                .description(description.clone())
                .ok_text("Delete")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let path = path.clone();
                    let _ = this.update(cx, |_, cx| {
                        let job_path = path.clone();
                        cx.spawn_in(window, async move |this, cx| {
                            let result = cx
                                .background_spawn(async move { if folder { std::fs::remove_dir_all(&job_path) } else { std::fs::remove_file(&job_path) } })
                                .await;
                            let _ = this.update_in(cx, |task, window, cx| match result {
                                Ok(()) => task.deleted(&path, window, cx),
                                Err(e) => window.push_notification(Notification::error(format!("Not deleted: {e}")), cx),
                            });
                        })
                        .detach();
                    });
                    true
                })
        });
    }

    /// After a delete: the tabs of it close, unless they hold what is not saved.
    fn deleted(&mut self, path: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        let gone: Vec<usize> = self
            .tabs
            .iter()
            .filter(|t| match &t.kind {
                TabKind::File(f) => f.read(cx).path.starts_with(path) && !f.read(cx).dirty,
                TabKind::Binary(b) => b.read(cx).path.starts_with(path),
                _ => false,
            })
            .map(|t| t.id)
            .collect();
        for id in gone {
            if let Some(ix) = self.ix_of(id) {
                self.remove_tab(ix, window, cx);
            }
        }
        if let Ok(relative) = path.strip_prefix(&self.root).map(|p| p.to_string_lossy().replace('\\', "/")) {
            let under = format!("{relative}/");
            if self.made_folders.iter().any(|f| *f == relative || f.starts_with(&under)) {
                self.made_folders = Rc::new(self.made_folders.iter().filter(|f| **f != relative && !f.starts_with(&under)).cloned().collect());
                self.tree_stale = true;
                self.fill_tree(cx);
            }
        }
        let _ = self.refresh_tx.try_send(());
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{Listing, Mark, build_tree, folder_of, git, moved, name_ok, tree_marks};
    use gpui_kit::{SharedString, component::tree::TreeItem};
    use std::{
        collections::{HashMap, HashSet},
        rc::Rc,
    };

    fn state(change: git::Change) -> git::FileState {
        git::FileState { staged: false, unstaged: true, change }
    }

    #[test]
    fn a_folder_is_new_only_when_all_in_it_is() {
        let files: Vec<String> = ["a/new/one.rs", "a/new/two.rs", "a/old.rs", "b/added.rs", "b/new.rs"].map(String::from).to_vec();
        let states: HashMap<String, git::FileState> = [
            ("a/new/one.rs", git::Change::Untracked),
            ("a/new/two.rs", git::Change::Untracked),
            ("b/added.rs", git::Change::Added),
            ("b/new.rs", git::Change::Untracked),
        ]
        .map(|(path, change)| (path.to_string(), state(change)))
        .into();
        let marks = tree_marks(&states, &files);
        assert_eq!(marks.get("a/new/one.rs"), Some(&Mark::Untracked));
        assert_eq!(marks.get("a/new/"), Some(&Mark::Untracked));
        assert_eq!(marks.get("a/"), Some(&Mark::Modified), "a/old.rs is git's");
        assert_eq!(marks.get("b/"), Some(&Mark::Added));
        assert_eq!(marks.get("a/old.rs"), None);
    }

    #[test]
    fn ignored_folders_are_read_when_open() {
        let files: Vec<String> = ["src/main.rs", "web/app.js"].map(String::from).to_vec();
        let ignored: Vec<String> = ["target/", "web/node_modules/", ".env"].map(String::from).to_vec();
        let ids = |items: &[TreeItem]| items.iter().map(|i| i.id.to_string()).collect::<Vec<_>>();
        let mut expanded = HashSet::new();
        let gone: HashSet<String> = ["src/main.rs".to_string()].into();
        let (items, _) = build_tree(&files, &gone, &ignored, &[], &HashMap::new(), &expanded);
        assert_eq!(ids(&items), ["target/", "web/", ".env"], "a deleted file is not in the tree");
        let none = HashSet::new();
        let (items, unread) = build_tree(&files, &none, &ignored, &[], &HashMap::new(), &expanded);
        assert_eq!(ids(&items), ["src/", "target/", "web/", ".env"]);
        assert_eq!(ids(&items[1].children), ["target/\0"], "not read yet");
        assert!(unread.is_empty(), "nothing open");
        assert_eq!(ids(&items[2].children), ["web/node_modules/", "web/app.js"]);

        expanded.insert(SharedString::from("target/"));
        let (_, unread) = build_tree(&files, &none, &ignored, &[], &HashMap::new(), &expanded);
        assert_eq!(unread, [SharedString::from("target/")]);

        let listed: HashMap<SharedString, Listing> = [(SharedString::from("target/"), Rc::new(vec![("debug".into(), true), ("x.log".into(), false)]))].into();
        let (items, unread) = build_tree(&files, &none, &ignored, &[], &listed, &expanded);
        assert!(unread.is_empty());
        assert_eq!(ids(&items[1].children), ["target/debug/", "target/x.log"]);
        assert_eq!(ids(&items[1].children[0].children), ["target/debug/\0"]);
    }

    #[test]
    fn names_and_where_they_go() {
        assert_eq!(folder_of("src/task/"), "src/task");
        assert_eq!(folder_of("src/main.rs"), "src");
        assert_eq!(folder_of("README.md"), "");
        assert!(name_ok("a.rs", false).is_ok());
        assert!(name_ok("deep/a.rs", true).is_ok());
        assert!(name_ok("deep/a.rs", false).is_err());
        for bad in ["", "..", "../x", "a//b", "/etc/x", "a\\b", "a/"] {
            assert!(name_ok(bad, true).is_err(), "{bad:?}");
        }
        let p = std::path::Path::new;
        assert_eq!(moved(p("/r/a/b.rs"), p("/r/a"), p("/r/c")), Some(p("/r/c/b.rs").to_path_buf()));
        assert_eq!(moved(p("/r/a"), p("/r/a"), p("/r/c")), Some(p("/r/c").to_path_buf()));
        assert_eq!(moved(p("/r/ab.rs"), p("/r/a"), p("/r/c")), None);
    }

    #[test]
    fn folders_made_empty_are_in_the_tree() {
        let files: Vec<String> = ["src/main.rs"].map(String::from).to_vec();
        let made: Vec<String> = ["docs/notes".to_string(), "src/new".to_string()].to_vec();
        let ids = |items: &[TreeItem]| items.iter().map(|i| i.id.to_string()).collect::<Vec<_>>();
        let (items, _) = build_tree(&files, &HashSet::new(), &[], &made, &HashMap::new(), &HashSet::new());
        assert_eq!(ids(&items), ["docs/", "src/"]);
        assert_eq!(ids(&items[0].children), ["docs/notes/"]);
        assert_eq!(ids(&items[1].children), ["src/new/", "src/main.rs"]);
    }
}
