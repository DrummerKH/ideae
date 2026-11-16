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
use gpui_kit::component::notification::Notification;

mod helpers;
mod term;
mod file;
mod changes;
mod session;
mod change_tree;
mod services;
mod pr;
pub use helpers::*;
// Named, to come before the one of the same name in gpui's own glob.
use helpers::outline;
pub use term::*;
pub use file::*;
pub use changes::*;
pub use session::*;
use change_tree::*;
use pr::*;

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
    /// Each file's part of the patch, hashed: what tells the diff view which
    /// files are as they were.
    uncommitted_keys: Rc<HashMap<String, u64>>,
    branch_keys: Rc<HashMap<String, u64>>,
    /// The files the branch changed since its base, lock files aside.
    branch_paths: Rc<BTreeSet<String>>,
    /// The single changes of each, to take back or stage one alone.
    uncommitted_hunks: Rc<Vec<git::Hunk>>,
    /// Files with conflict markers left in them, and whether committing
    /// them as they are was asked for a second time.
    conflict_marked: Vec<(String, u32)>,
    markers_accepted: bool,
    /// Work put away in the repository, the latest first.
    stashes: Vec<git::Stash>,
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
    /// Kept for as long as the task: the refresh, off the UI thread, tells
    /// it what to watch.
    _watching: Arc<std::sync::Mutex<Watching>>,
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
        let watching = Arc::new(std::sync::Mutex::new(Watching { watcher, watched: Default::default() }));
        let poll_watching = watching.clone();
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
                let Ok(light) = this.read_with(cx, |this, _| !this.visible) else { break };
                let watching = poll_watching.clone();
                let (read, patch, branch, shared, staged, on_disk, now_seen, now_known) = cx
                    .background_spawn(async move {
                        let mut read = git::refresh(&root, picked, light, &mut known);
                        // Here, not on the UI thread: a call for every directory that came or went.
                        watching.lock().unwrap().sync(std::mem::take(&mut read.watch));
                        // None of them when the patches are those of the refresh before.
                        let (patch, branch, shared, staged) = match read.patches.take() {
                            Some(git::Patches { working, branch, staged }) => {
                                // Nothing committed on the branch yet: one patch, parsed
                                // once and kept once (`shared`).
                                let same = branch == working;
                                let patch = parse_patch(working, &mut seen.0);
                                let (branch, shared) = match &patch {
                                    Some(_) if same => {
                                        seen.1 = seen.0;
                                        (None, true)
                                    }
                                    _ => (parse_patch(branch, &mut seen.1), false),
                                };
                                let staged: HashSet<(String, String)> = git::hunks(&staged).into_iter().map(|hunk| (hunk.path, hunk.changes)).collect();
                                (patch, branch, shared, Some(staged))
                            }
                            None => (None, None, false, None),
                        };
                        (read, patch, branch, shared, staged, changed_on_disk(open_known), seen, known)
                    })
                    .await;
                let git::Refreshed { files, info, states, base, stashes, failed, .. } = read;
                known = now_known;
                seen = now_seen;
                let applied = this.update_in(cx, |this, window, cx| {
                    for (file, change) in open.iter().zip(on_disk) {
                        if let Some(change) = change {
                            let _ = file.update(cx, |f, cx| f.apply_disk(change, window, cx));
                        }
                    }
                    // Git did not answer: what was read before stays, rather
                    // than an empty tree and no changes in its place.
                    if failed {
                        return;
                    }
                    if this.file_states != states {
                        this.file_states = states;
                        cx.notify();
                    }
                    if this.stashes != stashes {
                        this.stashes = stashes;
                        cx.notify();
                    }
                    if let Some(patch) = patch {
                        this.apply_patch(patch, cx);
                    }
                    if let Some(staged) = staged.filter(|staged| this.changes.read(cx).staged.as_ref() != staged) {
                        this.changes.update(cx, |changes, cx| {
                            changes.staged = Rc::new(staged);
                            cx.notify();
                        });
                    }
                    this.apply_branch_patch(base, branch, shared, cx);
                    this.apply_files(files, cx);
                    this.apply_branch(info, cx);
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
            uncommitted_keys: Rc::default(),
            branch_keys: Rc::default(),
            branch_paths: Rc::default(),
            uncommitted_hunks: Rc::default(),
            conflict_marked: Vec::new(),
            markers_accepted: false,
            stashes: Vec::new(),
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
            _watching: watching,
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
                    Some(ix) if this.tabs[ix].title != "agent" => {
                        // Its thread and what it kept of the screen go with the tab.
                        state.update(cx, |state, cx| state.close(cx));
                        this.remove_tab(ix, window, cx)
                    }
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
        self.show_diff_if_seen(cx);
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
        self.show_diff_if_seen(cx);
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
    /// The repository's branches, to choose one and say what to do with
    /// it; and all the merged ones at once, to tidy up.
    pub fn branches_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const MERGED: &str = "\u{1}merged";
        let (root, task, here) = (self.root.clone(), cx.weak_entity(), self.branch_info.head.clone());
        cx.spawn_in(window, async move |_, cx| {
            let from = root.clone();
            let rows = cx.background_spawn(async move { git::branch_rows(&from) }).await;
            let detail = crate::finder::DETAIL;
            // Merged, here, in no task and not the one in hand: nothing is lost with these.
            let spent: Vec<git::BranchRow> = rows.iter().filter(|r| r.merged && !r.remote && r.held.is_none() && r.name != here).cloned().collect();
            let mut items = Vec::new();
            if !spent.is_empty() {
                let text = format!("Delete the {} merged local branch{}…{detail}all of what they hold is in the branch work starts from", spent.len(), if spent.len() == 1 { "" } else { "es" });
                items.push(crate::finder::Hit { text, path: MERGED.into(), line: 0, col: 0, ranges: Vec::new() });
            }
            items.extend(rows.iter().map(|row| {
                let mut what = vec![if row.remote { "remote" } else { "local" }.to_string()];
                if row.name == here {
                    what.push("this task".into());
                } else if let Some(held) = &row.held {
                    what.push(format!("in {}", held.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
                }
                if row.merged {
                    what.push("merged".into());
                }
                what.extend([row.when.clone(), row.subject.clone()]);
                crate::finder::Hit { text: format!("{}{detail}{}", row.name, what.join(" · ")), path: row.name.clone(), line: 0, col: 0, ranges: Vec::new() }
            }));
            let picked = task.clone();
            let _ = cx.update(|window, cx| {
                crate::finder::Finder::open_pick(
                    "Branches…".into(),
                    root,
                    items,
                    Rc::new(move |name, window, cx| {
                        let _ = picked.update(cx, |task, cx| {
                            if name == MERGED {
                                return task.delete_branches_dialog(spent.clone(), window, cx);
                            }
                            if let Some(row) = rows.iter().find(|row| row.name == name) {
                                task.branch_dialog(row.clone(), window, cx)
                            }
                        });
                    }),
                    task,
                    window,
                    cx,
                )
            });
        })
        .detach();
    }

    /// What can be done with one branch.
    fn branch_dialog(&mut self, branch: git::BranchRow, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let here = branch.name == self.branch_info.head;
        window.open_dialog(cx, move |dialog, _, cx| {
            let (this, branch) = (this.clone(), branch.clone());
            let muted = cx.theme().muted_foreground;
            dialog.title(format!("⎇ {}", branch.name)).w(px(520.)).content(move |content, _, _| {
                let act = |id: &'static str, label: String, tip: &'static str, run: Rc<dyn Fn(&mut TaskView, git::BranchRow, &mut Window, &mut Context<TaskView>)>| {
                    let (this, branch) = (this.clone(), branch.clone());
                    Button::new(id).small().outline().label(label).tooltip(tip).on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let (run, branch) = (run.clone(), branch.clone());
                        let _ = this.update(cx, |task, cx| run(task, branch, window, cx));
                    })
                };
                let held = branch.held.as_ref().map(|path| path.display().to_string());
                content
                    .child(div().text_sm().text_color(muted).child(format!("{} · {}", branch.when, branch.subject)))
                    .children(held.clone().map(|path| div().text_xs().text_color(muted).child(format!("Checked out in {path}: a task has it."))))
                    .child(
                        h_flex()
                            .gap_2()
                            .pt_3()
                            .flex_wrap()
                            .when(held.is_none(), |row| {
                                row.child(act(
                                    "branch-open",
                                    "Open as task".into(),
                                    "A worktree of its own for this branch: it shows up as a task",
                                    Rc::new(|task, branch, window, cx| {
                                        let copy_env = crate::state::settings().copy_env;
                                        task.run_action("Open", move |root| git::branch_worktree(&root, &branch, copy_env).map(|dir| format!("Opened in {}", dir.display())), false, window, cx)
                                    }),
                                ))
                            })
                            .child(act(
                                "branch-compare",
                                "Compare with working tree".into(),
                                "What is here that the branch does not have",
                                Rc::new(|task, branch, window, cx| task.compare_with(branch.name, None, window, cx)),
                            ))
                            .when(held.is_none() && !here, |row| {
                                row.child(act(
                                    "branch-delete",
                                    if branch.remote { "Delete on the remote…".into() } else { "Delete…".into() },
                                    "Asked once more before it goes, unless nothing would be lost",
                                    Rc::new(|task, branch, window, cx| task.delete_branches_dialog(vec![branch], window, cx)),
                                ))
                            }),
                    )
            })
        });
    }

    /// Deletes branches: at once when they are local and merged, asked
    /// first when something goes that is nowhere else, or goes for everyone.
    fn delete_branches_dialog(&mut self, branches: Vec<git::BranchRow>, window: &mut Window, cx: &mut Context<Self>) {
        let delete = |task: &mut TaskView, branches: Vec<git::BranchRow>, window: &mut Window, cx: &mut Context<TaskView>| {
            task.run_action(
                "Delete",
                move |root| {
                    let mut gone = 0;
                    for branch in &branches {
                        // What was asked about is deleted as it is; what is merged needs no force.
                        git::delete_branch(&root, branch, !branch.merged)?;
                        gone += 1;
                    }
                    Ok(format!("Deleted {gone} branch{}", if gone == 1 { "" } else { "es" }))
                },
                false,
                window,
                cx,
            )
        };
        let one_safe = branches.len() == 1 && branches[0].merged && !branches[0].remote;
        if one_safe {
            return delete(self, branches, window, cx);
        }
        let this = cx.weak_entity();
        let names = branches.iter().map(|b| b.name.clone()).collect::<Vec<_>>().join(", ");
        let why = match branches.as_slice() {
            [one] if one.remote => "It is deleted on the remote, for everyone who uses it.",
            [one] if !one.merged => "It is not merged: its commits are kept nowhere else.",
            _ => "All of what they hold is in the branch work starts from.",
        };
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, branches) = (this.clone(), branches.clone());
            dialog
                .confirm()
                .title(if branches.len() == 1 { format!("Delete {names}?") } else { format!("Delete {} branches?", branches.len()) })
                .description(if branches.len() == 1 { why.to_string() } else { format!("{names}\n\n{why}") })
                .ok_text("Delete")
                .on_ok(move |_, window, cx| {
                    let branches = branches.clone();
                    let _ = this.update(cx, |task, cx| delete(task, branches, window, cx));
                    true
                })
        });
    }

    /// Asks what to compare the worktree with, or `file` of it alone: a
    /// branch, a tag, one of the latest commits.
    pub fn compare_with_dialog(&mut self, file: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let (root, task) = (self.root.clone(), cx.weak_entity());
        cx.spawn_in(window, async move |_, cx| {
            let from = root.clone();
            let refs = cx.background_spawn(async move { git::refs(&from) }).await;
            let items = refs
                .into_iter()
                .map(|(name, what)| crate::finder::Hit { text: format!("{name}{}{what}", crate::finder::DETAIL), path: name, line: 0, col: 0, ranges: Vec::new() })
                .collect();
            let title = match &file {
                Some(file) => format!("Compare {file} with…"),
                None => "Compare the working tree with…".to_string(),
            };
            let picked = task.clone();
            let _ = cx.update(|window, cx| {
                crate::finder::Finder::open_pick(
                    title,
                    root,
                    items,
                    Rc::new(move |rev, window, cx| {
                        let file = file.clone();
                        let _ = picked.update(cx, |task, cx| task.compare_with(rev, file, window, cx));
                    }),
                    task,
                    window,
                    cx,
                )
            });
        })
        .detach();
    }

    /// What the worktree has that `rev` has not, in a diff of its own.
    pub fn compare_with(&mut self, rev: String, file: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.root.clone();
        cx.spawn_in(window, async move |_, cx| {
            let title = match &file {
                Some(file) => format!("{file}: {rev} on the left, as it is now on the right"),
                None => format!("{rev} on the left, the working tree on the right"),
            };
            let files = cx
                .background_spawn(async move { DiffFile::parse(&git::diff_against(&root, &rev, file.as_deref())).map(with_languages).unwrap_or_default() })
                .await;
            let _ = cx.update(|window, cx| show_diff_dialog(title, files, window, cx));
        })
        .detach();
    }

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

    /// The open files with edits not on disk, but for the one that asks
    /// (which is being updated, and cannot be read).
    fn unsaved_files(&self, but: EntityId, cx: &App) -> Vec<PathBuf> {
        self.tabs
            .iter()
            .filter_map(|tab| match &tab.kind {
                TabKind::File(file) if file.entity_id() != but => Some(file.read(cx)).filter(|file| file.dirty).map(|file| file.path.clone()),
                _ => None,
            })
            .collect()
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

    /// What of the agent's work the task and its row show, to tell a report
    /// that changes it from one that does not.
    pub fn agent_shown(&self) -> (Option<SharedString>, Option<SharedString>, usize) {
        (self.agent_activity.clone().map(SharedString::from), self.agent_editing.clone(), self.agent_touched.len())
    }

    pub fn set_agent_status(&mut self, status: crate::agent::AgentStatus, seen: bool, cx: &mut Context<Self>) {
        // An agent at work reports every tool it uses: drawn again only for
        // what shows another way than before.
        let before = (self.unseen, self.agent_activity.is_some(), self.agent_editing.is_some());
        self.unseen = status.is_attention() && !seen;
        if status != crate::agent::AgentStatus::Working {
            self.agent_activity = None;
            self.agent_editing = None;
        }
        let changed = status != self.agent_status || before != (self.unseen, self.agent_activity.is_some(), self.agent_editing.is_some());
        self.agent_status = status;
        if changed {
            cx.emit(TaskEvent::Changed);
            cx.notify();
        }
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

    /// The files this task changed since it left its base, as its refresh
    /// read them; None on the base branch itself.
    pub fn touched(&self, cx: &App) -> Option<BTreeSet<String>> {
        self.changes.read(cx).base.as_ref().map(|_| (*self.branch_paths).clone())
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
        self.fill_tree(cx);
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

    /// Whether the keyboard is with the tab in front, for tests.
    #[cfg(test)]
    pub fn front_tab_has_focus(&self, window: &Window, cx: &App) -> bool {
        match self.focused_ix().and_then(|ix| self.tabs.get(ix)).map(|t| &t.kind) {
            Some(TabKind::Terminal(view)) => view.read(cx).state.read(cx).focus_handle().is_focused(window),
            Some(TabKind::File(file)) => file.read(cx).editor.read(cx).focus_handle(cx).is_focused(window),
            Some(TabKind::Browser(view)) => view.focus_handle(cx).is_focused(window),
            _ => self.focus.is_focused(window),
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        // A task out of view has nothing the keyboard can go to: asked for
        // there (a tab restored at the start, a prompt sent as its agent's turn
        // ends), the focus would leave the task that is typed in for nothing
        // that is drawn. It takes the focus when it comes into view.
        if !self.visible {
            return;
        }
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
        // The diff laid out and coloured goes with its tab; it is made
        // again from what the task keeps when the tab is next looked at.
        if matches!(removed.kind, TabKind::Changes) {
            let diff = self.changes.read(cx).diff.clone();
            diff.update(cx, |state, cx| state.set_files([], cx));
            self.changes.update(cx, |changes, _| (changes.hunks, changes.first_lines) = Default::default());
            self.diff_stale = true;
        }
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
        // The tab that comes to the front may be Changes, with an older diff.
        self.show_diff_if_seen(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn apply_patch(&mut self, parsed: ParsedPatch, cx: &mut Context<Self>) {
        let ParsedPatch { files, first_lines, hashes, hunks, marked } = parsed;
        self.uncommitted_hunks = Rc::new(hunks);
        // Never 0, which is no key to the diff view.
        self.uncommitted_keys = Rc::new(hashes.iter().map(|(path, hash)| (path.clone(), *hash | 1)).collect());
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
    /// `shared`: the branch has no commit of its own, and its diff is the
    /// uncommitted one just applied, kept once for both.
    fn apply_branch_patch(&mut self, base: Option<String>, parsed: Option<ParsedPatch>, shared: bool, cx: &mut Context<Self>) {
        let base_changed = self.changes.read(cx).base != base;
        if parsed.is_none() && !shared && !base_changed {
            return;
        }
        let paths = |files: &[DiffFile]| Rc::new(files.iter().map(|f| f.path().to_string()).filter(|path| !git::is_lockfile(path)).collect());
        if shared {
            self.branch_paths = paths(&self.uncommitted_diff);
            self.branch_diff = self.uncommitted_diff.clone();
            (self.branch_lines, self.branch_hunks, self.branch_keys) = (self.uncommitted_lines.clone(), self.uncommitted_hunks.clone(), self.uncommitted_keys.clone());
        } else if let Some(parsed) = parsed {
            self.branch_paths = paths(&parsed.files);
            self.branch_diff = parsed.files;
            self.branch_lines = Rc::new(parsed.first_lines);
            self.branch_hunks = Rc::new(parsed.hunks);
            self.branch_keys = Rc::new(parsed.hashes.into_iter().map(|(path, hash)| (path, hash | 1)).collect());
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
        let keys = match scope {
            DiffScope::Uncommitted => self.uncommitted_keys.clone(),
            DiffScope::Branch => self.branch_keys.clone(),
        };
        let diff = self.changes.read(cx).diff.clone();
        // The same diff a moment later: it stays where it is read.
        diff.update(cx, |state, cx| {
            let keyed = files.into_iter().map(|file| {
                let key = keys.get(file.path().as_ref()).copied().unwrap_or(0);
                (file, key)
            });
            state.update_files(keyed, cx)
        });
        self.changes.update(cx, |c, cx| {
            // Lines picked in a file that is another patch now are not those lines any more.
            let key_of = |keys: &HashMap<String, u64>, range: &DiffLineRange| keys.get(range.path().as_ref()).copied();
            if c.selection.as_ref().is_some_and(|range| key_of(&c.shown_keys, range) != key_of(&keys, range)) {
                c.selection = None;
                c.selected_code.clear();
            }
            c.shown_keys = keys;
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
        let shown = self.visible && self.right == RightPane::Files;
        if self.tree_stale && shown {
            self.tree_stale = false;
            let items = build_tree(&self.tree_files, &self.expanded);
            self.tree.update(cx, |t, cx| t.set_items(items, cx));
        } else if !self.tree_stale && !shown {
            // Let go of when not shown: every path again, in every task that showed it once.
            self.tree_stale = true;
            self.tree.update(cx, |t, cx| t.set_items(Vec::new(), cx));
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
            .child(
                div()
                    .id("branch-name")
                    .font_semibold()
                    .cursor_pointer()
                    .hover(|s| s.text_color(cx.theme().primary))
                    .child(format!("⎇ {}", if b.head.is_empty() { "…" } else { &b.head }))
                    .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("The repository's branches: open one as a task, compare, delete").build(window, cx))
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.branches_dialog(window, cx)
                    })),
            )
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
                            format!("Would conflict with your work in {}: not tried, a click asks the agent to take it in", b.pull_conflicts.join(", "))
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            // Known to conflict: not tried, the agent is asked to merge it.
                            let files = this.branch_info.pull_conflicts.clone();
                            if !files.is_empty() {
                                let list = files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n");
                                let upstream = this.branch_info.upstream.clone().unwrap_or_else(|| "the upstream".into());
                                this.paste_to_agent(
                                    format!("Take in {upstream} (fetch, then rebase or merge as this branch does). With the work here, committed or not, it conflicts in:\n{list}\n\nResolve the conflicts keeping what both sides meant, and check that it builds and the tests pass."),
                                    window,
                                    cx,
                                );
                                this.action = Some((false, format!("Pull: {} file(s) would conflict, nothing changed; the agent's prompt has a request", files.len())));
                                return cx.notify();
                            }
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
                let visible = self.visible;
                file.update(cx, |f, cx| if visible { f.go_to_line(line, col, window, cx) } else { f.jump(line, col, window, cx) });
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
                                this.fill_tree(cx);
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
                                this.fill_tree(cx);
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
                                this.fill_tree(cx);
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
                        Button::new("stash")
                            .small()
                            .ghost()
                            .disabled(busy || self.files.is_empty())
                            .label("Stash")
                            .tooltip("Put everything not committed away, to bring back later; the message box names it")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let message = this.commit_input.read(cx).value().trim().to_string();
                                this.run_action("Stash", move |root| git::stash(&root, &message), false, window, cx)
                            })),
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
        if !self.stashes.is_empty() {
            let muted = cx.theme().muted_foreground;
            list = list.child(
                div().px_3().pt_3().pb_1().text_xs().text_color(muted).child(format!("STASHES ({})", self.stashes.len())),
            );
            for (i, stash) in self.stashes.iter().enumerate() {
                let group = SharedString::from(format!("stash-{i}"));
                let act = |id: &'static str, label: &'static str, tip: &'static str, name: String, cx: &mut Context<Self>| {
                    Button::new((id, i)).xsmall().ghost().label(label).tooltip(tip).on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        let name = name.clone();
                        match id {
                            "stash-apply" => this.run_action("Stash", move |root| git::stash_apply(&root, &name, false), false, window, cx),
                            "stash-pop" => this.run_action("Stash", move |root| git::stash_apply(&root, &name, true), false, window, cx),
                            _ => this.drop_stash_dialog(name, window, cx),
                        }
                    }))
                };
                let name = stash.name.clone();
                list = list.child(
                    h_flex()
                        .id(group.clone())
                        .group(group.clone())
                        .relative()
                        .px_3()
                        .py_0p5()
                        .gap_2()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().accent))
                        .child(div().flex_1().min_w_0().truncate().child(stash.what.clone()))
                        .child(div().flex_none().text_xs().text_color(muted).child(stash.when.clone()))
                        // Over the row's end while the pointer is on it: the name keeps its room.
                        .child(
                            h_flex()
                                .absolute()
                                .right_1()
                                .top_0()
                                .bottom_0()
                                .bg(cx.theme().accent)
                                .opacity(0.)
                                .group_hover(group, |s| s.opacity(1.))
                                .child(act("stash-apply", "apply", "Bring it back and keep it in the list", stash.name.clone(), cx))
                                .child(act("stash-pop", "pop", "Bring it back and take it off the list", stash.name.clone(), cx))
                                .child(act("stash-drop", "drop", "Throw it away", stash.name.clone(), cx)),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| this.show_stash(name.clone(), window, cx))),
                );
            }
        }
        list.overflow_y_scrollbar()
    }

    /// What a stash holds, as a diff in a dialog.
    fn show_stash(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let what = self.stashes.iter().find(|s| s.name == name).map(|s| s.what.clone()).unwrap_or_default();
        cx.spawn_in(window, async move |_, cx| {
            let title = format!("{name} · {what}");
            let files = cx.background_spawn(async move { DiffFile::parse(&git::stash_patch(&root, &name)).map(with_languages).unwrap_or_default() }).await;
            let _ = cx.update(|window, cx| show_diff_dialog(title, files, window, cx));
        })
        .detach();
    }

    fn drop_stash_dialog(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let what = self.stashes.iter().find(|s| s.name == name).map(|s| s.what.clone()).unwrap_or_default();
        let this = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, name) = (this.clone(), name.clone());
            dialog
                .confirm()
                .title(format!("Drop {name}?"))
                .description(format!("{what}\n\nWhat it holds is gone for good."))
                .ok_text("Drop")
                .on_ok(move |_, window, cx| {
                    let name = name.clone();
                    let _ = this.update(cx, |this, cx| this.run_action("Stash", move |root| git::stash_drop(&root, &name), false, window, cx));
                    true
                })
        });
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
                git::apply_hunk(&root, &hunk.patch(), true, false).map(|_| format!("Took a change of {} back", hunk.path))
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
        self.save_all_now(cx);
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
    /// What History does with a commit: 0 takes it into this branch, 1
    /// commits its opposite, 2 starts a task from it. A conflict changes
    /// nothing and goes to the agent as a request.
    pub fn commit_action(&mut self, what: u8, hash: String, window: &mut Window, cx: &mut Context<Self>) {
        let short = hash[..hash.len().min(8)].to_string();
        match what {
            2 => {
                let copy_env = crate::state::settings().copy_env;
                self.run_action("Open", move |root| git::commit_worktree(&root, &hash, copy_env).map(|dir| format!("Opened in {}", dir.display())), false, window, cx)
            }
            revert => {
                let revert = revert == 1;
                let (label, asked) = if revert { ("Revert", format!("Revert commit {short} (`git revert --no-edit {short}`).")) } else { ("Cherry-pick", format!("Cherry-pick commit {short} into this branch (`git cherry-pick {short}`).")) };
                self.run_or_ask_agent(label, move |root| git::replay_commit(&root, &hash, revert), asked, window, cx)
            }
        }
    }

    /// Runs a git job that can meet a conflict. On one, what it did is
    /// undone and the agent's prompt gets the request to do it and resolve
    /// the conflicts: `asked` says what, the files that conflict are added.
    fn run_or_ask_agent(
        &mut self,
        label: &'static str,
        job: impl FnOnce(PathBuf) -> Result<String, git::RebaseError> + Send + 'static,
        asked: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        // On disk before git reads the files: a save still on its way would
        // be left out of a commit, or come back after a discard.
        self.save_all_now(cx);
        self.busy = Some(label);
        self.action = None;
        cx.notify();
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(crate::procs::on_thread(move || job(root))).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                this.action = Some(match result {
                    Ok(out) => (true, out),
                    Err(git::RebaseError::Failed(e)) => (false, format!("{label}: {e}")),
                    Err(git::RebaseError::Conflicts(files)) => {
                        let list = files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n");
                        this.paste_to_agent(
                            format!("{asked} It conflicts in:\n{list}\n\nResolve the conflicts keeping what both sides meant, check that it builds and the tests pass, then finish it."),
                            window,
                            cx,
                        );
                        (false, format!("{label}: {} conflicting file(s), nothing changed; the agent's prompt has a request", files.len()))
                    }
                });
                let _ = this.refresh_tx.try_send(());
                cx.notify();
            });
        })
        .detach();
    }

    /// Rebase onto the base; conflicts go to the agent as a request.
    fn rebase_onto_base(&mut self, base: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.save_all_now(cx);
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
        // On disk before git reads the files: a save still on its way would
        // be left out of a commit, or come back after a discard.
        self.save_all_now(cx);
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
            .on_action(cx.listener(|this, _: &ShowBranches, window, cx| this.branches_dialog(window, cx)))
            .on_action(cx.listener(|this, _: &CompareFileWith, window, cx| {
                let Some(TabKind::File(file)) = this.focused_ix().map(|ix| &this.tabs[ix].kind) else { return };
                let Ok(file) = file.read(cx).path.strip_prefix(&this.root).map(|path| path.to_string_lossy().into_owned()) else { return };
                this.compare_with_dialog(Some(file), window, cx)
            }))
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

        // The Changes tab in front has the diff as it is; behind the file's
        // tab it is left as it was, and given the new one when it comes to
        // the front again, however it does: here by the file's tab closing.
        let shown = |cx: &mut TestAppContext| task.update(cx, |t, cx| t.changes.read(cx).diff.read(cx).files().len());
        cx.update_window(handle.into(), |_, window, cx| task.update(cx, |t, cx| t.show_tab(|k| matches!(k, TabKind::Changes), window, cx))).unwrap();
        refreshed(cx);
        task.update(cx, |t, _| assert!(!t.diff_stale));
        assert_eq!(shown(cx), 1, "the one changed file");
        // Another file's tab, opened after it and so to its right, comes in front.
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();
        cx.update_window(handle.into(), |_, window, cx| task.update(cx, |t, cx| t.open_file(repo.join("new.txt"), 0, window, cx))).unwrap();
        refreshed(cx);
        task.update(cx, |t, _| assert!(t.diff_stale && t.files.len() == 2, "read, and not laid out behind the file's tab"));
        assert_eq!(shown(cx), 1);
        cx.update_window(handle.into(), |_, window, cx| {
            task.update(cx, |t, cx| {
                let ix = t.tabs.iter().rposition(|tab| matches!(tab.kind, TabKind::File(_))).unwrap();
                t.remove_tab(ix, window, cx);
            })
        })
        .unwrap();
        task.update(cx, |t, _| assert!(!t.diff_stale, "in front again, with the diff of now"));
        assert_eq!(shown(cx), 2);

        task.update(cx, |t, cx| t.shut_down(cx));
        cx.update_window(handle.into(), |_, window, _| window.remove_window()).ok();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod diff_update_tests {
    use gpui_kit::component::diff::{Diff, DiffFile, DiffState};
    use gpui_kit::{AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, Window, div, px};

    struct Host {
        state: Entity<DiffState>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(400.)).h(px(200.)).child(Diff::new(&self.state).size_full())
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    /// A file of 200 lines, the first ones changed to `tag`.
    fn file(name: &str, tag: &str) -> (DiffFile, u64) {
        let lines: String = (0..200).map(|ix| format!(" line {ix}\n")).collect();
        let patch = format!("diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n@@ -1,201 +1,201 @@\n-old\n+{tag}\n{lines}");
        (DiffFile::parse(&patch).unwrap().remove(0), super::text_hash(&patch) | 1)
    }

    /// The Changes tab while an agent writes: a new patch leaves the view at
    /// the file it was reading, where taking it as another diff goes to the top.
    #[gpui_kit::test]
    fn a_diff_that_changes_stays_where_it_is_read(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_cn::init(cx);
            crate::theme::init(cx);
        });
        let state = cx.new(|cx| DiffState::new([], cx));
        let (_, cx) = cx.add_window_view({
            let state = state.clone();
            move |_, _| Host { state }
        });
        cx.update(|_, cx| state.update(cx, |state, cx| state.update_files([file("a.txt", "one"), file("b.txt", "one"), file("c.txt", "one")], cx)));
        for _ in 0..3 {
            draw(cx);
        }
        // To the third file: its header comes a third down the view, the end of the second above it.
        cx.update(|_, cx| state.update(cx, |state, cx| state.scroll_to_file("c.txt", cx)));
        draw(cx);
        let top = |cx: &mut VisualTestContext| cx.update(|_, cx| state.read(cx).top_file().map(|path| path.to_string()));
        assert_eq!(top(cx).as_deref(), Some("b.txt"));

        // b.txt changes again, the others are as they were, d.txt is new.
        cx.update(|_, cx| {
            state.update(cx, |state, cx| state.update_files([file("a.txt", "one"), file("b.txt", "two"), file("c.txt", "one"), file("d.txt", "one")], cx))
        });
        draw(cx);
        assert_eq!(top(cx).as_deref(), Some("b.txt"), "still at the lines being read");
        // And the file before it goes.
        cx.update(|_, cx| state.update(cx, |state, cx| state.update_files([file("b.txt", "two"), file("c.txt", "one"), file("d.txt", "one")], cx)));
        draw(cx);
        assert_eq!(top(cx).as_deref(), Some("b.txt"));

        // Another diff altogether starts at its top.
        cx.update(|_, cx| state.update(cx, |state, cx| state.set_files([file("a.txt", "one").0, file("b.txt", "one").0], cx)));
        draw(cx);
        assert_eq!(top(cx).as_deref(), Some("a.txt"));
    }
}
