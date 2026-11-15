//! The window: projects and their tasks in the sidebar, the task in view, and
//! the state saved between runs.

use std::path::{Path, PathBuf};

use gpui_kit::component::{
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    notification::Notification,
    h_flex, v_flex,
};
use gpui_kit::*;

use crate::{
    agent, git, naming,
    state::{self, ProjectState, State, TaskSession, TaskState},
    task::{SidePanelWidth, TaskEvent, TaskView},
    theme::{self, Appearance},
};

pub struct TaskEntry {
    pub path: PathBuf,
    pub branch: String,
    pub title: String,
    /// The project's own worktree: one per project, never deleted.
    pub main: bool,
    pub starred: bool,
    pub last_active: u64,
    /// The tabs to open when the task starts again.
    session: TaskSession,
    /// Running: its terminals, agent and editors. Kept while it is out of
    /// view, so coming back finds everything as it was.
    pub view: Option<Entity<TaskView>>,
    /// A git operation on it is running (creating, deleting).
    pub busy: Option<SharedString>,
    /// Locked worktree (`git worktree lock`) whose locker is gone.
    pub locked: Option<String>,
}

impl TaskEntry {
    pub fn label(&self) -> String {
        if self.title.is_empty() { self.branch.clone() } else { self.title.clone() }
    }
}

pub struct Project {
    pub path: PathBuf,
    pub name: String,
    pub collapsed: bool,
    pub tasks: Vec<TaskEntry>,
}

impl Project {
    fn new(path: PathBuf) -> Self {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        Self { path, name, collapsed: false, tasks: Vec::new() }
    }
}

pub struct Workspace {
    pub projects: Vec<Project>,
    /// The task in view, by worktree path.
    pub active: Option<PathBuf>,
    pub active_collapsed: bool,
    agent: Option<String>,
    hook_socket: Option<PathBuf>,
    /// Kept alive for the app's lifetime: dropping it removes the socket.
    _bridge: Option<agent::Bridge>,
    pub appearance: Appearance,
    pub sidebar_width: Pixels,
    /// In the GNOME dash; asked once, not while the menu opens.
    pub pinned: bool,
    pub sysmon: Entity<crate::sysmon::SysMon>,
    /// The state as last written, to write only changes.
    last_saved: String,
    _subscriptions: Vec<Subscription>,
    _tick: Task<()>,
}

impl Workspace {
    pub fn new(
        repos: Vec<PathBuf>,
        agent: Option<String>,
        appearance: Option<Appearance>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let saved = state::load();
        let appearance = appearance
            .or_else(|| saved.appearance.as_deref().and_then(Appearance::parse))
            .unwrap_or(Appearance::Auto);
        theme::apply(appearance, window, cx);
        if let Some(width) = saved.side_panel_width {
            cx.set_global(SidePanelWidth(px(width)));
        }
        crate::zoom::init(saved.zoom, cx);

        let (hook_tx, hook_rx) = async_channel::unbounded::<agent::HookEvent>();
        let bridge = agent::Bridge::start(move |event| {
            let _ = hook_tx.send_blocking(event);
        })
        .inspect_err(|e| eprintln!("agent hooks off: {e}"))
        .ok();
        // Claude gets the hooks; any other agent command runs as given.
        let agent = match (agent, &bridge) {
            (Some(cmd), Some(bridge)) if cmd == "claude" => Some(bridge.claude_command("claude")),
            (agent, _) => agent,
        };
        let hook_socket = bridge.as_ref().map(|b| b.socket.clone());
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = hook_rx.recv().await {
                if this.update_in(cx, |this, window, cx| this.on_hook(event, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let subscriptions = vec![
            cx.observe_window_appearance(window, |this: &mut Self, window, cx| {
                if this.appearance == Appearance::Auto {
                    theme::apply(Appearance::Auto, window, cx);
                }
            }),
            // Leaving the window saves edited files, as JetBrains IDEs do.
            cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if !window.is_window_active() {
                    for view in this.views() {
                        view.update(cx, |t, cx| t.autosave_all(cx));
                    }
                }
            }),
            // Every theme change, from the switcher or the OS, recolors the terminals.
            cx.observe_global::<gpui_kit::component::Theme>(|this: &mut Self, cx| {
                let colors = theme::terminal_colors(cx);
                for view in this.views() {
                    view.update(cx, |t, cx| t.set_terminal_colors(&colors, cx));
                }
            }),
            // Terminals end their whole session (the agent and whatever it
            // started in the background); nothing of a task outlives the app.
            cx.on_app_quit(|this: &mut Self, cx| {
                this.save(cx);
                for view in this.views() {
                    view.update(cx, |t, cx| t.shut_down(cx));
                }
                crate::procs::end_child_sessions();
                async {}
            }),
        ];

        // Saves now and then, and picks up worktrees made outside the app.
        let tick = cx.spawn_in(window, async move |this, cx| {
            let mut n = 0u32;
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                n += 1;
                let alive = this.update_in(cx, |this, window, cx| {
                    this.save(cx);
                    if n % 10 == 0 {
                        this.sync_worktrees(window, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        let mut this = Self {
            projects: Vec::new(),
            active: None,
            active_collapsed: saved.active_collapsed,
            agent,
            hook_socket,
            _bridge: bridge,
            appearance,
            sidebar_width: px(saved.sidebar_width.unwrap_or(260.)),
            pinned: crate::desktop::is_pinned(),
            sysmon: {
                let mon = cx.new(crate::sysmon::SysMon::new);
                cx.observe(&mon, |_, _, cx| cx.notify()).detach();
                mon
            },
            last_saved: String::new(),
            _subscriptions: subscriptions,
            _tick: tick,
        };
        this.restore(&saved, window, cx);

        // Repositories named on the command line join the list; the first one
        // comes into view when nothing else is.
        let mut first_cli = None;
        let repos = if repos.is_empty() && this.projects.is_empty() {
            std::env::current_dir().ok().filter(|d| git::main_worktree(d).is_some()).into_iter().collect()
        } else {
            repos
        };
        for repo in repos {
            if let Some(ix) = this.add_project(&repo) {
                first_cli.get_or_insert(ix);
            }
        }
        if this.active.is_none() {
            if let Some(ix) = first_cli {
                this.open_main(ix, window, cx);
            }
        }
        this.sync_worktrees(window, cx);

        if let Some(view) = this.active_view() {
            view.update(cx, |t, cx| {
                // Debug aid for headless screenshots: IDEAE_OPEN=path:line.
                if let Ok(spec) = std::env::var("IDEAE_OPEN") {
                    let (path, line) = spec.rsplit_once(':').unwrap_or((spec.as_str(), "1"));
                    let line = line.parse::<u32>().unwrap_or(1).saturating_sub(1);
                    t.open_file(path.into(), line, window, cx);
                }
                // Debug aid for headless screenshots: IDEAE_URL opens a browser tab.
                if let Ok(url) = std::env::var("IDEAE_URL") {
                    t.open_browser(&url, window, cx);
                }
            });
        }
        this
    }

    fn restore(&mut self, saved: &State, window: &mut Window, cx: &mut Context<Self>) {
        for p in &saved.projects {
            let mut project = Project::new(p.path.clone());
            project.collapsed = p.collapsed;
            project.tasks = p
                .tasks
                .iter()
                .map(|t| TaskEntry {
                    path: t.path.clone(),
                    branch: t.branch.clone(),
                    title: t.title.clone(),
                    main: t.main,
                    starred: t.starred,
                    last_active: t.last_active,
                    session: t.session.clone(),
                    view: None,
                    busy: None,
                    locked: None,
                })
                .collect();
            self.projects.push(project);
        }
        let open: Vec<PathBuf> = saved
            .projects
            .iter()
            .flat_map(|p| &p.tasks)
            .filter(|t| t.open && t.path.exists())
            .map(|t| t.path.clone())
            .collect();
        for path in open {
            self.load(&path, window, cx);
        }
        if let Some(active) = saved.active_task.as_ref().filter(|p| self.find(p).is_some()) {
            self.switch(active, window, cx);
        }
    }

    // ------------------------------------------------------------ lookup

    pub fn find(&self, path: &Path) -> Option<(usize, usize)> {
        self.projects.iter().enumerate().find_map(|(pi, p)| {
            p.tasks.iter().position(|t| t.path == path).map(|ti| (pi, ti))
        })
    }

    fn entry_mut(&mut self, path: &Path) -> Option<&mut TaskEntry> {
        let (pi, ti) = self.find(path)?;
        Some(&mut self.projects[pi].tasks[ti])
    }

    pub fn views(&self) -> Vec<Entity<TaskView>> {
        self.projects.iter().flat_map(|p| &p.tasks).filter_map(|t| t.view.clone()).collect()
    }

    pub fn active_view(&self) -> Option<Entity<TaskView>> {
        let (pi, ti) = self.find(self.active.as_ref()?)?;
        self.projects[pi].tasks[ti].view.clone()
    }

    pub fn active_project(&self) -> Option<usize> {
        self.active.as_ref().and_then(|a| self.find(a)).map(|(pi, _)| pi)
    }

    // ------------------------------------------------------------ state

    fn snapshot(&self, cx: &App) -> State {
        State {
            appearance: Some(self.appearance.label().to_lowercase()),
            sidebar_width: Some(f32::from(self.sidebar_width)),
            side_panel_width: cx.try_global::<SidePanelWidth>().map(|w| f32::from(w.0)),
            active_collapsed: self.active_collapsed,
            active_task: self.active.clone(),
            zoom: crate::zoom::current(cx),
            projects: self
                .projects
                .iter()
                .map(|p| ProjectState {
                    path: p.path.clone(),
                    collapsed: p.collapsed,
                    tasks: p
                        .tasks
                        .iter()
                        .filter(|t| t.busy.is_none() || t.view.is_some())
                        .map(|t| TaskState {
                            path: t.path.clone(),
                            branch: t.branch.clone(),
                            title: t.title.clone(),
                            main: t.main,
                            starred: t.starred,
                            last_active: t.last_active,
                            open: t.view.is_some(),
                            session: match &t.view {
                                Some(view) => view.read(cx).session(cx),
                                None => t.session.clone(),
                            },
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn serialize(&self, cx: &App) -> String {
        serde_json::to_string_pretty(&self.snapshot(cx)).unwrap_or_default()
    }

    pub fn save(&mut self, cx: &App) {
        let json = self.serialize(cx);
        if json != self.last_saved {
            match state::save(&json) {
                Ok(()) => self.last_saved = json,
                Err(e) => eprintln!("saving state: {e}"),
            }
        }
    }

    // ------------------------------------------------------------ tasks

    /// Starts a task: its terminals, agent and watchers.
    fn load(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<TaskView>> {
        let (agent, socket) = (self.agent.clone(), self.hook_socket.clone());
        let entry = self.entry_mut(path)?;
        if let Some(view) = &entry.view {
            return Some(view.clone());
        }
        if entry.busy.is_some() {
            return None;
        }
        let (root, branch, session) = (entry.path.clone(), entry.branch.clone(), entry.session.clone());
        let view = cx.new(|cx| {
            let mut t = TaskView::new(root, branch, agent, socket, window, cx);
            t.restore(&session, window, cx);
            t
        });
        cx.subscribe(&view, |_, _, _: &TaskEvent, cx| cx.notify()).detach();
        self.entry_mut(path)?.view = Some(view.clone());
        Some(view)
    }

    pub fn switch(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.as_deref() != Some(path) {
            if let Some(old) = self.active_view() {
                old.update(cx, |t, cx| {
                    t.autosave_all(cx);
                    t.set_visible(false, window, cx)
                });
            }
        }
        let Some(view) = self.load(path, window, cx) else { return };
        self.active = Some(path.to_path_buf());
        if let Some(entry) = self.entry_mut(path) {
            entry.last_active = state::now();
        }
        view.update(cx, |t, cx| {
            t.unseen = false;
            t.set_visible(true, window, cx)
        });
        cx.notify();
    }

    /// Stops a task; its worktree and tabs stay for the next start.
    pub fn close_task(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.find(path).and_then(|(pi, ti)| self.projects[pi].tasks[ti].view.clone()) else {
            return;
        };
        if view.read(cx).agent_busy() {
            let this = cx.weak_entity();
            let path = path.to_path_buf();
            window.open_alert_dialog(cx, move |dialog, _, _| {
                let (this, path) = (this.clone(), path.clone());
                dialog
                    .confirm()
                    .title("Stop the agent?")
                    .description("It is in the middle of a turn. The task's terminals are stopped too.")
                    .ok_text("Stop")
                    .on_ok(move |_, window, cx| {
                        let _ = this.update(cx, |this, cx| this.stop(&path, window, cx));
                        true
                    })
            });
            return;
        }
        self.stop(path, window, cx);
    }

    fn stop(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        let entry = &mut self.projects[pi].tasks[ti];
        let Some(view) = entry.view.take() else { return };
        entry.session = view.read(cx).session(cx);
        view.update(cx, |t, cx| t.shut_down(cx));
        // The main thread shows only while it runs.
        if entry.main {
            self.projects[pi].tasks.remove(ti);
        }
        if self.active.as_deref() == Some(path) {
            self.active = None;
            let next = self.projects.iter().flat_map(|p| &p.tasks).filter(|t| t.view.is_some()).max_by_key(|t| t.last_active);
            if let Some(next) = next.map(|t| t.path.clone()) {
                self.switch(&next, window, cx);
            }
        }
        cx.notify();
    }

    /// Removes the task's worktree and branch, after a look at what would be lost.
    pub fn delete_task(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        let entry = &self.projects[pi].tasks[ti];
        if entry.main || entry.busy.is_some() {
            return;
        }
        let (dirty, unpushed) = git::unsaved_work(path);
        let mut description = format!("Removes the worktree {} and the branch {}.", path.display(), entry.branch);
        if dirty > 0 {
            description.push_str(&format!("\n{dirty} uncommitted file(s) will be lost."));
        }
        if unpushed > 0 {
            description.push_str(&format!("\n{unpushed} commit(s) are on no remote and will be lost."));
        }
        if let Some(reason) = entry.locked.as_ref() {
            let reason = if reason.is_empty() { String::new() } else { format!(" ({reason})") };
            description.push_str(&format!("\nThe worktree is locked{reason}, by nothing running now; the lock is overridden."));
        }
        let title = format!("Delete \"{}\"?", entry.label());
        let this = cx.weak_entity();
        let path = path.to_path_buf();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, path) = (this.clone(), path.clone());
            dialog
                .confirm()
                .title(title.clone())
                .description(description.clone())
                .ok_text("Delete")
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.remove_task(&path, window, cx));
                    true
                })
        });
    }

    fn remove_task(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.stop(path, window, cx);
        let Some((pi, ti)) = self.find(path) else { return };
        let repo = self.projects[pi].path.clone();
        let entry = &mut self.projects[pi].tasks[ti];
        entry.busy = Some("deleting…".into());
        let (path, branch, unlock) = (path.to_path_buf(), entry.branch.clone(), entry.locked.is_some());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let task_path = path.clone();
            let result = cx
                .background_spawn(async move { git::remove_task_worktree(&repo, &task_path, &branch, unlock) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        if let Some((pi, ti)) = this.find(&path) {
                            this.projects[pi].tasks.remove(ti);
                        }
                    }
                    Err(e) => {
                        if let Some(entry) = this.entry_mut(&path) {
                            entry.busy = None;
                        }
                        window.push_notification(Notification::error(e).title("Delete failed"), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn toggle_star(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(entry) = self.entry_mut(path) {
            entry.starred = !entry.starred;
        }
        cx.notify();
    }

    /// Opens the project's own worktree as a task, adding it to the list.
    pub fn open_main(&mut self, project: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get_mut(project) else { return };
        if !p.tasks.iter().any(|t| t.main) {
            let branch = git::worktrees(&p.path).into_iter().next().map(|w| w.branch).unwrap_or_default();
            p.tasks.insert(
                0,
                TaskEntry {
                    path: p.path.clone(),
                    branch,
                    title: String::new(),
                    main: true,
                    starred: false,
                    last_active: 0,
                    session: TaskSession::default(),
                    view: None,
                    busy: None,
                    locked: None,
                },
            );
        }
        p.collapsed = false;
        let path = p.path.clone();
        self.switch(&path, window, cx);
        if let Some(view) = self.active_view() {
            view.update(cx, |t, cx| t.show_view(0, window, cx));
        }
    }

    /// Asks for a title, then makes the branch and worktree.
    pub fn new_task_dialog(&mut self, project: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(project) else { return };
        let name = p.name.clone();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("What is it about? DEV-123 Fix login, or plain words"));
        let this = cx.weak_entity();
        let submit = {
            let (this, input) = (this.clone(), input.clone());
            move |window: &mut Window, cx: &mut App| {
                let title = input.read(cx).value().trim().to_string();
                if title.is_empty() {
                    return;
                }
                window.close_dialog(cx);
                let _ = this.update(cx, |this, cx| this.create_task(project, title, window, cx));
            }
        };
        // Runs inside this entity's update, so it cannot go through `submit`.
        cx.subscribe_in(&input, window, move |this, input, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let title = input.read(cx).value().trim().to_string();
                if !title.is_empty() {
                    window.close_dialog(cx);
                    this.create_task(project, title, window, cx);
                }
            }
        })
        .detach();
        let focused = std::rc::Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, submit, focused) = (input.clone(), submit.clone(), focused.clone());
            dialog
                .title(format!("New task in {name}"))
                .w(px(560.))
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let input = input.clone();
                        window.defer(cx, move |window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
                    }
                    content.child(
                        v_flex()
                            .gap_2()
                            .child(Input::new(&input))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(branch_hint(&input.read(cx).value())),
                            ),
                    )
                })
                .footer({
                    let submit = submit.clone();
                    h_flex().justify_end().gap_2().child(
                        Button::new("create-task").primary().label("Create").on_click(move |_, window, cx| submit(window, cx)),
                    )
                })
        });
    }

    fn create_task(&mut self, project: usize, title: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get_mut(project) else { return };
        let branch = naming::branch_name(&title);
        let repo = p.path.clone();
        let path = git::task_worktree_dir(&repo, &branch);
        p.collapsed = false;
        p.tasks.push(TaskEntry {
            path: path.clone(),
            branch: branch.clone(),
            title,
            main: false,
            starred: false,
            last_active: state::now(),
            session: TaskSession::default(),
            view: None,
            busy: Some("creating…".into()),
            locked: None,
        });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { git::create_task_worktree(&repo, &branch) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => {
                        if let Some(entry) = this.entry_mut(&path) {
                            entry.busy = None;
                        }
                        this.switch(&path, window, cx);
                    }
                    Err(e) => {
                        if let Some((pi, ti)) = this.find(&path) {
                            this.projects[pi].tasks.remove(ti);
                        }
                        window.push_notification(Notification::error(e).title("Could not create the task"), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ------------------------------------------------------------ projects

    /// Adds a repository (by any path inside it) and returns its index.
    pub fn add_project(&mut self, path: &Path) -> Option<usize> {
        let path = std::fs::canonicalize(path).ok()?;
        let root = git::main_worktree(&path).unwrap_or(path);
        if let Some(ix) = self.projects.iter().position(|p| p.path == root) {
            return Some(ix);
        }
        self.projects.push(Project::new(root));
        Some(self.projects.len() - 1)
    }

    pub fn add_project_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                for path in paths {
                    this.add_project(&path);
                }
                this.sync_worktrees(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn remove_project(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(ix) else { return };
        let running: Vec<PathBuf> = p.tasks.iter().filter(|t| t.view.is_some()).map(|t| t.path.clone()).collect();
        let name = p.name.clone();
        let path = p.path.clone();
        let this = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, running, path) = (this.clone(), running.clone(), path.clone());
            dialog
                .confirm()
                .title(format!("Remove {name} from the list?"))
                .description("Its files and worktrees stay on disk; running tasks are stopped.")
                .ok_text("Remove")
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| {
                        for task in &running {
                            this.stop(task, window, cx);
                        }
                        this.projects.retain(|p| p.path != path);
                        cx.notify();
                    });
                    true
                })
        });
    }

    /// Brings the task list in line with `git worktree list`: worktrees made
    /// outside the app appear, removed ones go.
    pub fn sync_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repos: Vec<PathBuf> = self.projects.iter().map(|p| p.path.clone()).collect();
        cx.spawn_in(window, async move |this, cx| {
            let lists = cx
                .background_spawn(async move {
                    repos.into_iter().map(|r| (git::worktrees(&r), r)).collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (worktrees, repo) in lists {
                    let Some(p) = this.projects.iter_mut().find(|p| p.path == repo) else { continue };
                    // Not readable now (unmounted, git missing): leave it be.
                    if worktrees.first().is_none_or(|w| w.path != repo) {
                        continue;
                    }
                    // Worktrees locked by a live process (Claude Code subagents)
                    // are that process's for now: not tasks.
                    let worktrees: Vec<_> = worktrees.into_iter().enumerate().filter(|(i, w)| *i == 0 || !w.in_use()).map(|(_, w)| w).collect();
                    for wt in worktrees.iter().skip(1) {
                        match p.tasks.iter_mut().find(|t| t.path == wt.path) {
                            Some(t) => {
                                t.branch = wt.branch.clone();
                                t.locked = wt.locked.clone();
                            }
                            None => p.tasks.push(TaskEntry {
                                path: wt.path.clone(),
                                branch: wt.branch.clone(),
                                title: String::new(),
                                main: false,
                                starred: false,
                                last_active: 0,
                                session: TaskSession::default(),
                                view: None,
                                busy: None,
                                locked: wt.locked.clone(),
                            }),
                        }
                    }
                    if let Some(main) = p.tasks.iter_mut().find(|t| t.main) {
                        main.branch = worktrees[0].branch.clone();
                    }
                    p.tasks.retain(|t| {
                        t.main || t.busy.is_some() || t.view.is_some() || worktrees.iter().any(|w| w.path == t.path)
                    });
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ------------------------------------------------------------ order

    pub fn move_project(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.projects.len() || to > self.projects.len() {
            return;
        }
        let p = self.projects.remove(from);
        let to = if to > from { to - 1 } else { to };
        self.projects.insert(to.min(self.projects.len()), p);
        cx.notify();
    }

    /// Moves a task within its project to just before `to` (or the end).
    pub fn move_task(&mut self, path: &Path, project: usize, to: Option<usize>, cx: &mut Context<Self>) {
        let Some((pi, from)) = self.find(path) else { return };
        if pi != project {
            return;
        }
        let tasks = &mut self.projects[pi].tasks;
        let task = tasks.remove(from);
        let to = to.map_or(tasks.len(), |to| if to > from { to - 1 } else { to }).min(tasks.len());
        tasks.insert(to, task);
        cx.notify();
    }

    // ------------------------------------------------------------ hooks

    fn on_hook(&mut self, event: agent::HookEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = event.status() else { return };
        let Some((pi, ti)) = self.find(&event.task) else { return };
        let entry = &self.projects[pi].tasks[ti];
        let Some(view) = entry.view.clone() else { return };
        let label = format!("{} · {}", self.projects[pi].name, entry.label());
        let seen = self.active.as_deref() == Some(event.task.as_path()) && window.is_window_active();
        let was = view.read(cx).agent_status.clone();
        view.update(cx, |t, cx| t.set_agent_status(status.clone(), seen, cx));
        if status.is_attention() && !seen && status != was {
            let body = match &status {
                agent::AgentStatus::NeedsInput(message) => message.clone(),
                _ => "Finished".to_string(),
            };
            // Waited for off the UI thread, or each one stays a zombie.
            if let Ok(mut child) = std::process::Command::new("notify-send").args(["-a", state::APP_ID, &label, &body]).spawn() {
                std::thread::spawn(move || child.wait());
            }
        }
        cx.notify();
    }

    pub fn set_appearance(&mut self, appearance: Appearance, window: &mut Window, cx: &mut Context<Self>) {
        self.appearance = appearance;
        theme::apply(appearance, window, cx);
        cx.notify();
    }
}

fn branch_hint(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        "The branch name comes from the title.".into()
    } else {
        let branch = naming::preview(title);
        format!("Branch: {branch}")
    }
}

use gpui_kit::component::ActiveTheme as _;
