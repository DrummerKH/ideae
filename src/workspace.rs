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
    /// When it was last in view, this run; its language servers sleep after
    /// a while out of view.
    pub left_view: Option<std::time::Instant>,
    /// Its pull request, and when `gh` was last asked.
    pub pr: Option<crate::pr::Pr>,
    pub pr_checked: Option<std::time::Instant>,
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
    /// The agent given on the command line, over Settings.
    agent_override: Option<Option<String>>,
    hook_socket: Option<String>,
    /// Kept alive for the app's lifetime: dropping it removes the socket.
    _bridge: Option<agent::Bridge>,
    pub appearance: Appearance,
    pub sidebar_width: Pixels,
    /// In the GNOME dash; asked once, not while the menu opens.
    pub pinned: bool,
    pub sysmon: Entity<crate::sysmon::SysMon>,
    /// Filters the sidebar's tasks.
    pub search: Entity<InputState>,
    /// The state as last written, to write only changes.
    last_saved: String,
    _subscriptions: Vec<Subscription>,
    _tick: Task<()>,
    pub build: crate::build_watch::BuildWatch,
    /// Where the window is, to open there next time.
    window_place: Option<state::WindowPlace>,
    /// Where the window was asked to open, and when: some window managers
    /// place it a fixed offset away (frame or shadow insets), which would
    /// otherwise add up with every restart.
    requested_origin: Option<(Point<Pixels>, std::time::Instant)>,
    origin_offset: Point<Pixels>,
    /// Quitting while the agents go on: a restart, or the window's ×.
    keep_terminals: bool,
}

impl Workspace {
    pub fn new(
        repos: Vec<PathBuf>,
        agent: Option<Option<String>>,
        appearance: Option<Appearance>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let saved = state::load();
        let appearance = appearance
            .or_else(|| saved.appearance.as_deref().and_then(Appearance::parse))
            .unwrap_or(Appearance::Auto);
        theme::apply(appearance, window, cx);
        crate::lsp::POWER_SAVING.store(saved.power_saving, std::sync::atomic::Ordering::Relaxed);
        if let Some(width) = saved.side_panel_width {
            cx.set_global(SidePanelWidth(px(width)));
        }
        crate::zoom::init(saved.zoom, cx);

        let (hook_tx, hook_rx) = async_channel::unbounded::<agent::HookEvent>();
        let bridge_tx = hook_tx.clone();
        let bridge = agent::Bridge::start(move |event| {
            let _ = bridge_tx.send_blocking(event);
        })
        .inspect_err(|e| eprintln!("agent hooks off: {e}"))
        .ok();
        // Claude gets the hooks; any other agent command runs as given.
        let agent_override = agent;
        let agent = Self::agent_command(&agent_override, bridge.as_ref());
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut hook_socket = bridge.as_ref().map(|b| b.socket.clone());
        // Terminals run in the daemon, which also takes the hooks. Without
        // one (Windows, or it would not start) they run in the window.
        #[cfg(unix)]
        if std::env::var_os("IDEAE_NO_DAEMON").is_none() {
            let live = hook_tx.clone();
            let to_event = |h: crate::daemon::proto::Hook, replayed: bool| agent::HookEvent {
                task: h.task.into(),
                event: h.event,
                message: h.message,
                kind: h.kind,
                replayed,
            };
            match crate::daemon::client::connect(move |h| {
                let _ = live.send_blocking(to_event(h, false));
            }) {
                Some((_, earlier)) => {
                    hook_socket = Some(crate::daemon::proto::socket_path().display().to_string());
                    for h in earlier {
                        let _ = hook_tx.send_blocking(to_event(h, true));
                    }
                }
                None => eprintln!("daemon unavailable: terminals run in the window"),
            }
        }
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
            // A restart leaves the daemon's terminals running for the next
            // window; any other quit ends them with everything else.
            cx.on_app_quit(|this: &mut Self, cx| {
                this.save(cx);
                if this.keep_terminals {
                    for view in this.views() {
                        view.update(cx, |t, cx| t.autosave_all(cx));
                    }
                } else {
                    for view in this.views() {
                        view.update(cx, |t, cx| t.shut_down(cx));
                    }
                    #[cfg(unix)]
                    if let Some(daemon) = crate::daemon::client::get() {
                        daemon.shutdown();
                    }
                }
                crate::procs::end_child_sessions();
                async {}
            }),
        ];

        cx.observe_window_bounds(window, |this: &mut Self, window, _| {
            let (bounds, maximized, fullscreen) = match window.window_bounds() {
                WindowBounds::Windowed(b) => (b, false, false),
                WindowBounds::Maximized(b) => (b, true, false),
                WindowBounds::Fullscreen(b) => (b, false, true),
            };
            if let Some((asked, at)) = this.requested_origin {
                let offset = bounds.origin - asked;
                let small = offset.x.abs() < px(150.) && offset.y.abs() < px(150.);
                if at.elapsed() < std::time::Duration::from_secs(3) && !maximized && small {
                    this.origin_offset = offset;
                }
            }
            let origin = bounds.origin - this.origin_offset;
            this.window_place = Some(state::WindowPlace {
                x: f32::from(origin.x),
                y: f32::from(origin.y),
                width: f32::from(bounds.size.width),
                height: f32::from(bounds.size.height),
                maximized,
                fullscreen,
            });
        })
        .detach();

        // Closing the window the window manager's way is the same as ×.
        let closing = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = closing.update(cx, |w, cx| w.close_window(cx));
            true
        });

        // Saves now and then, and picks up worktrees made outside the app.
        let tick = cx.spawn_in(window, async move |this, cx| {
            let mut n = 0u32;
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                n += 1;
                let alive = this.update_in(cx, |this, window, cx| {
                    this.save(cx);
                    this.check_build(window, cx);
                    if n % 10 == 0 {
                        this.sync_worktrees(window, cx);
                        this.sleep_idle_language_servers(cx);
                        this.refresh_prs(false, window, cx);
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
            agent_override,
            hook_socket,
            _bridge: bridge,
            appearance,
            sidebar_width: px(saved.sidebar_width.unwrap_or(260.)),
            pinned: crate::desktop::is_pinned(),
            search: {
                let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search tasks…"));
                cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify()).detach();
                search
            },
            sysmon: {
                let mon = cx.new(crate::sysmon::SysMon::new);
                cx.observe(&mon, |_, _, cx| cx.notify()).detach();
                mon
            },
            last_saved: String::new(),
            _subscriptions: subscriptions,
            _tick: tick,
            requested_origin: None,
            origin_offset: Point::default(),
            window_place: saved.window.clone(),
            keep_terminals: false,
            build: crate::build_watch::BuildWatch::new(),
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
        // Sessions no task shows any more (closed while the window was gone).
        #[cfg(unix)]
        if let Some(daemon) = crate::daemon::client::get() {
            let keep = this.views().iter().flat_map(|v| v.read(cx).sessions(cx)).collect();
            daemon.prune(keep);
        }

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
                    left_view: None,
                    pr: None,
                    pr_checked: None,
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
            power_saving: crate::lsp::POWER_SAVING.load(std::sync::atomic::Ordering::Relaxed),
            window: self.window_place.clone(),
            settings: state::settings(),
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
            // Servers start when the task comes into view (switch).
            t.lsp_asleep = true;
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
            if let Some(old) = self.active.clone().and_then(|a| self.entry_mut(&a)) {
                old.left_view = Some(std::time::Instant::now());
            }
        }
        let Some(view) = self.load(path, window, cx) else { return };
        self.active = Some(path.to_path_buf());
        if let Some(entry) = self.entry_mut(path) {
            entry.last_active = state::now();
        }
        view.update(cx, |t, cx| {
            t.unseen = false;
            t.wake_language_servers(cx);
            t.set_visible(true, window, cx)
        });
        self.refresh_prs(true, window, cx);
        cx.notify();
    }

    /// Asks `gh` about the PRs of tasks: running ones every two minutes,
    /// the rest every ten, merged and closed ones hourly; `now` for the
    /// task just shown.
    fn refresh_prs(&mut self, now: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::pr::PrState;
        if !state::settings().pr_status {
            return;
        }
        use std::time::Duration;
        let mut due = Vec::new();
        for p in &mut self.projects {
            for t in &mut p.tasks {
                if t.main || t.busy.is_some() {
                    continue;
                }
                let every = match &t.pr {
                    Some(pr) if matches!(pr.state, PrState::Merged | PrState::Closed) => Duration::from_secs(3600),
                    _ if t.view.is_some() => Duration::from_secs(120),
                    _ => Duration::from_secs(600),
                };
                let active = self.active.as_deref() == Some(t.path.as_path());
                let stale = t.pr_checked.is_none_or(|at| at.elapsed() >= every || (now && active && at.elapsed() > Duration::from_secs(20)));
                if stale && (!now || active) {
                    t.pr_checked = Some(std::time::Instant::now());
                    due.push((t.path.clone(), t.branch.clone()));
                }
            }
        }
        if due.is_empty() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    due.into_iter().map(|(path, branch)| {
                        let pr = crate::pr::lookup(&path, &branch);
                        (path, pr)
                    }).collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (path, pr) in found {
                    if let Some(entry) = this.entry_mut(&path) {
                        entry.pr = pr;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Deletes every task of the project whose PR was merged, after a list.
    pub fn clean_up_merged(&mut self, project: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(project) else { return };
        let merged: Vec<(PathBuf, String)> = p
            .tasks
            .iter()
            .filter(|t| !t.main && t.pr.as_ref().is_some_and(|pr| pr.state == crate::pr::PrState::Merged))
            .map(|t| (t.path.clone(), t.label()))
            .collect();
        if merged.is_empty() {
            window.push_notification(Notification::info("No task of this project has a merged PR."), cx);
            return;
        }
        let names = merged.iter().map(|(_, l)| format!("• {l}")).collect::<Vec<_>>().join("\n");
        let description = format!("Removes their worktrees and branches:\n{names}");
        let this = cx.weak_entity();
        let paths: Vec<PathBuf> = merged.into_iter().map(|(p, _)| p).collect();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, paths) = (this.clone(), paths.clone());
            dialog
                .confirm()
                .title(format!("Delete {} merged task{}?", paths.len(), if paths.len() == 1 { "" } else { "s" }))
                .description(description.clone())
                .ok_text("Delete")
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |w, cx| {
                        for path in &paths {
                            w.remove_task(path, window, cx);
                        }
                    });
                    true
                })
        });
    }

    /// Language servers of tasks out of view this long go to sleep;
    /// IDEAE_LSP_IDLE_SECS shortens it for tests.
    fn lsp_idle() -> std::time::Duration {
        let secs = std::env::var("IDEAE_LSP_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(u64::from(state::settings().lsp_idle_minutes.max(1)) * 60);
        std::time::Duration::from_secs(secs)
    }

    fn sleep_idle_language_servers(&mut self, cx: &mut Context<Self>) {
        for p in &self.projects {
            for t in &p.tasks {
                let (Some(view), Some(left)) = (&t.view, t.left_view) else { continue };
                if self.active.as_deref() != Some(t.path.as_path()) && left.elapsed() >= Self::lsp_idle() {
                    if view.read(cx).has_language_servers() {
                        view.update(cx, |t, cx| t.put_language_servers_to_sleep(cx));
                    }
                }
            }
        }
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
                    left_view: None,
                    pr: None,
                    pr_checked: None,
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
            left_view: None,
            pr: None,
            pr_checked: None,
        });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let copy_env = state::settings().copy_env;
            let result = cx.background_spawn(async move { git::create_task_worktree(&repo, &branch, copy_env) }).await;
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
                                left_view: None,
                                pr: None,
                                pr_checked: None,
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
        view.update(cx, |t, cx| {
            t.set_agent_status(status.clone(), seen, cx);
            match event.event.as_str() {
                "PreToolUse" => t.agent_activity = event.message.clone(),
                "UserPromptSubmit" => t.agent_activity = Some("Thinking".into()),
                _ => {}
            }
        });
        self.projects[pi].tasks[ti].last_active = state::now();
        if status.is_attention() && !seen && status != was && !event.replayed && state::settings().notifications {
            let body = match &status {
                agent::AgentStatus::NeedsInput(message) => message.clone(),
                _ => "Finished".to_string(),
            };
            crate::desktop::notify(&label, &body);
        }
        cx.notify();
    }

    /// Power saving: stops every language server and starts none until
    /// turned off, when the open files get theirs again.
    pub fn toggle_power_saving(&mut self, cx: &mut Context<Self>) {
        let on = !crate::lsp::POWER_SAVING.load(std::sync::atomic::Ordering::Relaxed);
        crate::lsp::POWER_SAVING.store(on, std::sync::atomic::Ordering::Relaxed);
        for view in self.views() {
            view.update(cx, |t, cx| t.set_language_servers(!on, cx));
        }
        cx.notify();
    }

    fn check_build(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(exe) = self.build.check() else { return };
        cx.spawn_in(window, async move |this, cx| {
            let version = cx.background_spawn(async move { crate::build_watch::probe(exe) }).await;
            let _ = this.update(cx, |this, cx| {
                this.build.found(version);
                cx.notify();
            });
        })
        .detach();
    }

    /// The agent tab's command line: the command line's choice, else the
    /// Settings one; Claude gets the hooks.
    fn agent_command(over: &Option<Option<String>>, bridge: Option<&agent::Bridge>) -> Option<String> {
        let cmd = match over {
            Some(cmd) => cmd.clone(),
            None => Some(state::settings().agent).filter(|a| !a.trim().is_empty()),
        };
        match (cmd, bridge) {
            (Some(cmd), Some(bridge)) if agent::is_claude(&cmd) => Some(bridge.claude_command(cmd.trim())),
            (cmd, _) => cmd,
        }
    }

    /// New settings: they go to the state, and what they change now
    /// changes (new tasks' agent, language servers of open files).
    pub fn apply_settings(&mut self, new: state::Settings, cx: &mut Context<Self>) {
        let old = state::settings();
        state::set_settings(new.clone());
        self.agent = Self::agent_command(&self.agent_override, self._bridge.as_ref());
        if old.lsp_off != new.lsp_off {
            for view in self.views() {
                view.update(cx, |t, cx| {
                    t.set_language_servers(false, cx);
                    if !crate::lsp::POWER_SAVING.load(std::sync::atomic::Ordering::Relaxed) {
                        t.set_language_servers(true, cx);
                    }
                });
            }
        }
        self.save(cx);
        cx.notify();
    }

    pub fn placed_at(&mut self, origin: Option<Point<Pixels>>) {
        self.requested_origin = origin.map(|o| (o, std::time::Instant::now()));
    }

    /// The window's ×: the window goes, the daemon's agents and terminals
    /// stay for the next one.
    pub fn close_window(&mut self, cx: &mut Context<Self>) {
        #[cfg(unix)]
        if crate::daemon::client::get().is_some() {
            self.keep_terminals = true;
        }
        self.save(cx);
        cx.quit();
    }

    /// Exit: ends every agent and terminal, after a word when any run.
    pub fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let running = self.views().len();
        if running == 0 {
            self.save(cx);
            return cx.quit();
        }
        let busy = self.views().iter().filter(|v| v.read(cx).agent_busy()).count();
        let mut description = format!(
            "Stops the agents and terminals of {running} running task{}",
            if running == 1 { "" } else { "s" }
        );
        if busy > 0 {
            description.push_str(&format!(", {busy} of them working right now"));
        }
        description.push_str(". Closing the window (×) keeps them running instead.");
        let this = cx.weak_entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            dialog.confirm().title("Quit ideae?").description(description.clone()).ok_text("Quit").on_ok(move |_, _, cx| {
                let _ = this.update(cx, |w, cx| {
                    w.keep_terminals = false;
                    w.save(cx);
                    cx.quit();
                });
                true
            })
        });
    }

    pub fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
        self.keep_terminals = true;
        match crate::desktop::spawn_again() {
            Ok(()) => cx.quit(),
            Err(e) => {
                self.keep_terminals = false;
                window.push_notification(Notification::error(e.to_string()).title("Restart"), cx)
            }
        }
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
