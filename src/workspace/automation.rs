//! What runs for a project's tasks besides their agents: the setup of a
//! new worktree, the teardown when a task is paused or archived, the check
//! when its agent finishes a turn. The commands of a repository's
//! `.ideae.toml` may be anyone's: they run once you allowed that version
//! of them. Also the hints that tell of all this when it would have helped,
//! and the window where a project's settings are set (Project settings…).

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    notification::Notification,
    v_flex,
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use super::Workspace;
use crate::{
    project_config::{self, Config, Resolved, Suggested, TaskSetup},
    services::Service,
    task::{AutoAsk, TaskEvent, TaskView},
};

/// How long a teardown may run on Pause and Archive; at quit, all of them
/// together, the window gone already.
const TEARDOWN: Duration = Duration::from_secs(60);
const TEARDOWN_AT_QUIT: Duration = Duration::from_secs(10);

type Then = Box<dyn FnOnce(&mut Workspace, bool, &mut Window, &mut Context<Workspace>)>;

#[derive(Default)]
pub struct AutoState {
    /// What waits for an answer about a file's commands, by their key: one
    /// question for all of them.
    asking: HashMap<String, Vec<Then>>,
    /// Teardowns that run, by worktree: done when the sender is dropped.
    tearing: HashMap<PathBuf, async_channel::Receiver<()>>,
    /// Hints given this run, by project.
    hinted: HashSet<(PathBuf, &'static str)>,
}

#[derive(Clone, Copy)]
enum Hint {
    Check,
    Setup,
}

impl Hint {
    fn key(self) -> &'static str {
        match self {
            Hint::Check => "check",
            Hint::Setup => "setup",
        }
    }
}

impl Workspace {
    /// What applies to the task at `path`, read off the UI thread.
    fn resolved(&self, path: &Path, window: &mut Window, cx: &mut Context<Self>, then: impl FnOnce(&mut Self, Resolved, &mut Window, &mut Context<Self>) + 'static) {
        let root = path.to_path_buf();
        cx.spawn_in(window, async move |this, cx| {
            let resolved = cx.background_spawn(async move { project_config::resolve(&root) }).await;
            let _ = this.update_in(cx, |this, window, cx| then(this, resolved, window, cx));
        })
        .detach();
    }

    /// `then` with whether the commands may run: those kept for yourself
    /// may; a file's once you said so for that version of them, asked the
    /// first time.
    fn trusting(&mut self, path: &Path, resolved: &Resolved, window: &mut Window, cx: &mut Context<Self>, then: impl FnOnce(&mut Self, bool, &mut Window, &mut Context<Self>) + 'static) {
        let key = project_config::trust_key(&resolved.config.task).filter(|_| resolved.from_file);
        let Some((key, pi)) = key.zip(self.find(path).map(|(pi, _)| pi)) else {
            return then(self, true, window, cx);
        };
        if let Some(&allowed) = self.projects[pi].trusted.get(&key) {
            return then(self, allowed, window, cx);
        }
        let waiting = self.auto.asking.entry(key.clone()).or_default();
        waiting.push(Box::new(then));
        if waiting.len() > 1 {
            return;
        }
        let project = self.projects[pi].path.clone();
        let name = self.projects[pi].name.clone();
        let task = resolved.config.task.clone();
        let this = cx.weak_entity();
        let answered = Rc::new(Cell::new(false));
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let answer = |allowed: Option<bool>| {
                let (this, project, key, answered) = (this.clone(), project.clone(), key.clone(), answered.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    if !answered.replace(true) {
                        let _ = this.update(cx, |w, cx| w.answer_trust(&project, &key, allowed, window, cx));
                    }
                    true
                }
            };
            let (on_close, on_cancel, on_ok) = (answer(None), answer(Some(false)), answer(Some(true)));
            dialog
                .confirm()
                .width(px(600.))
                .title("Run the commands this repository's .ideae.toml defines?")
                .description(commands_list(&name, &task, cx))
                .ok_text("Run them")
                .cancel_text("Don't run")
                .on_ok(on_ok)
                .on_cancel(on_cancel)
                .on_close(move |event, window, cx| {
                    on_close(event, window, cx);
                })
        });
    }

    /// Remembered unless the question was only put away (`None`): then it
    /// is asked again next time.
    fn answer_trust(&mut self, project: &Path, key: &str, allowed: Option<bool>, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(allowed), Some(p)) = (allowed, self.projects.iter_mut().find(|p| p.path == project)) {
            p.trusted.insert(key.to_string(), allowed);
        }
        for then in self.auto.asking.remove(key).unwrap_or_default() {
            then(self, allowed == Some(true), window, cx);
        }
        self.save_later(cx);
        cx.notify();
    }

    fn view_of(&self, path: &Path) -> Option<Entity<TaskView>> {
        let (pi, ti) = self.find(path)?;
        self.projects[pi].tasks[ti].view.clone()
    }

    /// A new worktree is there and its task runs: the setup starts in it,
    /// next to the agent. Without one, a project that installs something
    /// is told it could.
    pub(super) fn task_made(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, _)) = self.find(path) else { return };
        let (root, project) = (path.to_path_buf(), self.projects[pi].path.clone());
        cx.spawn_in(window, async move |this, cx| {
            let job_root = root.clone();
            let (resolved, installs) =
                cx.background_spawn(async move { (project_config::resolve(&job_root), !project_config::suggest(&project).setup.is_empty()) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(view) = this.view_of(&root) else { return };
                view.update(cx, |t, cx| t.set_configured(resolved.config.task.clone(), cx));
                let setup = resolved.config.task.setup.clone();
                if setup.is_empty() {
                    if installs && resolved.error.is_none() {
                        this.hint(&root, Hint::Setup, window, cx);
                    }
                    return;
                }
                this.trusting(&root, &resolved, window, cx, move |_, allowed, window, cx| {
                    if allowed {
                        view.update(cx, |t, cx| t.start_setup(setup, window, cx));
                    }
                });
            });
        })
        .detach();
    }

    /// An agent's turn ended: the project's check runs on what it left.
    pub(super) fn after_turn(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        if self.projects[pi].tasks[ti].side.is_some() {
            return;
        }
        let variant = !self.projects[pi].tasks[ti].group.is_empty();
        let root = path.to_path_buf();
        self.resolved(path, window, cx, move |this, resolved, window, cx| {
            let Some(view) = this.view_of(&root) else { return };
            view.update(cx, |t, cx| t.set_configured(resolved.config.task.clone(), cx));
            let check = resolved.config.task.check.trim().to_string();
            if check.is_empty() {
                // Variants have a check of their own, run in all of them.
                if !variant && resolved.error.is_none() {
                    this.hint(&root, Hint::Check, window, cx);
                }
                return;
            }
            this.trusting(&root, &resolved, window, cx, move |_, allowed, _, cx| {
                if allowed {
                    view.update(cx, |t, cx| t.start_check(check, false, cx));
                }
            });
        });
    }

    /// The Run panel asks for the settings, or a setup or a check again.
    pub(super) fn on_task_event(&mut self, view: &Entity<TaskView>, event: &TaskEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let TaskEvent::Heard(heard) = event {
            return self.on_hook(heard.clone(), window, cx);
        }
        let TaskEvent::Auto(ask) = event else {
            return cx.notify();
        };
        let root = view.read(cx).root.clone();
        let Some((pi, _)) = self.find(&root) else { return };
        match ask {
            AutoAsk::Settings => self.project_settings(pi, window, cx),
            AutoAsk::Setup | AutoAsk::Check => {
                let (ask, view) = (*ask, view.clone());
                self.resolved(&root.clone(), window, cx, move |this, resolved, window, cx| {
                    view.update(cx, |t, cx| t.set_configured(resolved.config.task.clone(), cx));
                    let task = resolved.config.task.clone();
                    this.trusting(&root, &resolved, window, cx, move |_, allowed, window, cx| {
                        if !allowed {
                            return;
                        }
                        view.update(cx, |t, cx| match ask {
                            AutoAsk::Setup => t.start_setup(task.setup, window, cx),
                            _ => t.start_check(task.check, true, cx),
                        });
                    });
                });
            }
        }
    }

    /// The task was paused or is being archived: its teardown runs in its
    /// worktree, bounded, the worktree kept until it is done.
    pub(super) fn teardown(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        let t = &self.projects[pi].tasks[ti];
        if t.main || t.side.is_some() || self.auto.tearing.contains_key(path) {
            return;
        }
        let slot = t.port_slot;
        let (done, waiting) = async_channel::bounded::<()>(1);
        self.auto.tearing.insert(path.to_path_buf(), waiting);
        let root = path.to_path_buf();
        self.resolved(path, window, cx, move |this, resolved, window, cx| {
            let commands = resolved.config.task.teardown.clone();
            if commands.is_empty() {
                this.auto.tearing.remove(&root);
                return drop(done);
            }
            let env = crate::services::task_env(&resolved.config.service, slot);
            this.trusting(&root.clone(), &resolved, window, cx, move |this, allowed, window, cx| {
                if !allowed {
                    this.auto.tearing.remove(&root);
                    return drop(done);
                }
                cx.spawn_in(window, async move |this, cx| {
                    let job_root = root.clone();
                    let outcome = cx
                        .background_spawn(crate::procs::on_thread(move || {
                            project_config::run(&commands, &job_root, &env, TEARDOWN, &std::sync::atomic::AtomicBool::new(false))
                        }))
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.auto.tearing.remove(&root);
                        if !outcome.ok() {
                            let why = if outcome.timed_out { format!("Stopped after {} seconds.", TEARDOWN.as_secs()) } else { project_config::tail(&outcome.output, 6) };
                            let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            window.push_notification(Notification::warning(why).title(format!("Teardown failed in {name}")), cx);
                        }
                    });
                    drop(done);
                })
                .detach();
            });
        });
    }

    /// Done when the task's teardown is, if one runs.
    pub(super) fn torn_down(&self, path: &Path) -> impl std::future::Future<Output = ()> + 'static {
        let waiting = self.auto.tearing.get(path).cloned();
        async move {
            if let Some(waiting) = waiting {
                let _ = waiting.recv().await;
            }
        }
    }

    /// Quitting ends the tasks that run: their teardowns run now, all at
    /// once, for a few seconds at most. Only those already allowed: nobody
    /// is there to ask.
    pub(super) fn teardown_at_quit(&mut self) {
        let mut runs = Vec::new();
        for p in &self.projects {
            for t in p.tasks.iter().filter(|t| t.view.is_some() && !t.main && t.side.is_none() && !self.auto.tearing.contains_key(&t.path)) {
                let resolved = project_config::resolve(&t.path);
                let commands = resolved.config.task.teardown.clone();
                let key = project_config::trust_key(&resolved.config.task);
                let allowed = !resolved.from_file || key.and_then(|k| p.trusted.get(&k).copied()).unwrap_or(false);
                if commands.is_empty() || !allowed {
                    continue;
                }
                let env = crate::services::task_env(&resolved.config.service, t.port_slot);
                let root = t.path.clone();
                runs.push(std::thread::spawn(move || {
                    project_config::run(&commands, &root, &env, TEARDOWN_AT_QUIT, &std::sync::atomic::AtomicBool::new(false));
                }));
            }
        }
        for run in runs {
            let _ = run.join();
        }
    }

    /// A word on what would have helped just now, once a run for each
    /// project, with "Don't show again".
    fn hint(&mut self, path: &Path, hint: Hint, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, _)) = self.find(path) else { return };
        let project = self.projects[pi].path.clone();
        if self.projects[pi].hints_off.contains(hint.key()) || !self.auto.hinted.insert((project.clone(), hint.key())) || self.presenting.is_some() {
            return;
        }
        let (title, text) = match hint {
            Hint::Check => (
                "Run tests automatically when an agent finishes?",
                "A check (cargo test, npm test…) runs in the task after each of its agent's turns; its row shows ✓ or ✗, and a failure goes back to the agent in a click.",
            ),
            Hint::Setup => (
                "Tasks start without dependencies installed",
                "A setup (npm ci, uv sync…) can run in each new task's worktree, next to its agent.",
            ),
        };
        let this = cx.weak_entity();
        let note = Notification::new().title(title).autohide(false).content(move |_, _, cx| {
            let (set_up, off) = (this.clone(), this.clone());
            let (project, project_off) = (project.clone(), project.clone());
            v_flex()
                .gap_2()
                .child(div().text_sm().text_color(cx.theme().muted_foreground).child(text))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("hint-set-up").small().primary().label("Set up…").on_click(cx.listener(move |note, _, window, cx| {
                            let project = project.clone();
                            let _ = set_up.update(cx, |w, cx| {
                                if let Some(pi) = w.projects.iter().position(|p| p.path == project) {
                                    w.project_settings(pi, window, cx);
                                }
                            });
                            note.dismiss(window, cx);
                        })))
                        .child(Button::new("hint-off").small().ghost().label("Don't show again").on_click(cx.listener(move |note, _, window, cx| {
                            let project = project_off.clone();
                            let _ = off.update(cx, |w, cx| {
                                if let Some(p) = w.projects.iter_mut().find(|p| p.path == project) {
                                    p.hints_off.insert(hint.key().to_string());
                                }
                                w.save_later(cx);
                            });
                            note.dismiss(window, cx);
                        }))),
                )
                .into_any_element()
        });
        window.push_notification(note, cx);
    }

    /// The project's agent, in its main checkout, is asked to write the
    /// settings; what it writes is reviewed there as any change.
    pub fn ask_agent_for_settings(&mut self, pi: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.projects.get(pi).map(|p| p.path.clone()) else { return };
        self.open_main(pi, window, cx);
        let Some(view) = self.view_of(&path) else { return };
        let text = AGENT_PROMPT.to_string();
        view.update(cx, |t, cx| {
            if !t.has_agent() {
                window.push_notification(Notification::warning("This task has no agent to ask.").title("Project settings"), cx);
            } else if t.agent_status == crate::agent::AgentStatus::Unknown {
                t.queue_prompt(text, true, window, cx);
            } else {
                t.message_agent(text, window, cx);
            }
        });
    }

    /// Reads the project's file and what its files suggest, then opens the
    /// settings over them.
    pub fn project_settings(&mut self, pi: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.projects.get(pi).map(|p| p.path.clone()) else { return };
        cx.spawn_in(window, async move |this, cx| {
            let job = project.clone();
            let (file, suggested) = cx.background_spawn(async move { (project_config::read_file(&job), project_config::suggest(&job)) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(pi) = this.projects.iter().position(|p| p.path == project) {
                    this.project_settings_dialog(pi, file, suggested, window, cx);
                }
            });
        })
        .detach();
    }

    fn project_settings_dialog(&mut self, pi: usize, file: Result<Option<Config>, String>, suggested: Suggested, window: &mut Window, cx: &mut Context<Self>) {
        let p = &self.projects[pi];
        let (project, name) = (p.path.clone(), p.name.clone());
        let own = project_config::own(&project);
        let (file_error, file) = match file {
            Ok(file) => (None, file),
            Err(e) => (Some(e), None),
        };
        let file_key = file.as_ref().and_then(|f| project_config::trust_key(&f.task));
        let file_trust = file_key.as_ref().and_then(|k| p.trusted.get(k).copied());
        let start = own.clone().or_else(|| file.clone()).unwrap_or_default();
        let only_me = Rc::new(Cell::new(own.is_some()));
        let lines = |list: &[String]| list.join("\n");
        let area = |value: String, hint: String, window: &mut Window, cx: &mut Context<Self>| {
            let state = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 6).placeholder(hint));
            state.update(cx, |s, cx| s.set_value(value, window, cx));
            state
        };
        let line = |value: String, hint: String, window: &mut Window, cx: &mut Context<Self>| {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(hint));
            state.update(cx, |s, cx| s.set_value(value, window, cx));
            state
        };
        // What the project suggests, or an example, told apart.
        let or = |s: String, example: &str| if s.is_empty() { format!("e.g. {example}") } else { s };
        let setup = area(lines(&start.task.setup), or(lines(&suggested.setup), "npm ci (one command a line)"), window, cx);
        let teardown = area(lines(&start.task.teardown), or(lines(&suggested.teardown), "docker compose down"), window, cx);
        let check = line(start.task.check.clone(), or(suggested.check.clone(), "cargo test"), window, cx);
        let copy_default = if crate::state::settings().copy_env { "the ignored .env* files, as Settings say" } else { "none" };
        let copy_hint = if suggested.copy.is_empty() { copy_default.to_string() } else { format!("{}; empty: {copy_default}", suggested.copy.join(" ")) };
        let copy = line(start.task.copy.clone().unwrap_or_default().join(" "), copy_hint, window, cx);
        let rows: Rc<RefCell<Vec<ServiceRow>>> = Rc::new(RefCell::new(Vec::new()));
        for service in &start.service {
            let row = ServiceRow::new_in(service.clone(), window, cx);
            rows.borrow_mut().push(row);
        }
        let fill = {
            let (setup, teardown, check, copy, rows, suggested) = (setup.clone(), teardown.clone(), check.clone(), copy.clone(), rows.clone(), suggested.clone());
            move |window: &mut Window, cx: &mut App| {
                let empty_then = |state: &Entity<TextareaState>, value: String, window: &mut Window, cx: &mut App| {
                    if state.read(cx).value().trim().is_empty() && !value.is_empty() {
                        state.update(cx, |s, cx| s.set_value(value, window, cx));
                    }
                };
                empty_then(&setup, suggested.setup.join("\n"), window, cx);
                empty_then(&teardown, suggested.teardown.join("\n"), window, cx);
                for (state, value) in [(&check, suggested.check.clone()), (&copy, suggested.copy.join(" "))] {
                    if state.read(cx).value().trim().is_empty() && !value.is_empty() {
                        state.update(cx, |s, cx| s.set_value(value, window, cx));
                    }
                }
                let known: HashSet<String> = rows.borrow().iter().map(|r| r.name.read(cx).value().trim().to_string()).collect();
                for service in suggested.services.iter().filter(|s| !known.contains(&s.name)) {
                    let row = ServiceRow::new_in(service.clone(), window, cx);
                    rows.borrow_mut().push(row);
                }
                window.refresh();
            }
        };
        let collect = {
            let (setup, teardown, check, copy, rows) = (setup.clone(), teardown.clone(), check.clone(), copy.clone(), rows.clone());
            move |cx: &App| -> Config {
                let split = |text: &str| text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect::<Vec<_>>();
                let copy = copy.read(cx).value().split([' ', ',']).map(str::trim).filter(|c| !c.is_empty()).map(str::to_string).collect::<Vec<_>>();
                Config {
                    task: TaskSetup {
                        setup: split(&setup.read(cx).value()),
                        teardown: split(&teardown.read(cx).value()),
                        check: check.read(cx).value().trim().to_string(),
                        copy: (!copy.is_empty()).then_some(copy),
                    },
                    service: rows.borrow().iter().filter_map(|r| r.service(cx)).collect(),
                }
            }
        };
        let this = cx.weak_entity();
        let save = {
            let (this, project, only_me, collect) = (this.clone(), project.clone(), only_me.clone(), collect.clone());
            move |window: &mut Window, cx: &mut App| {
                let config = collect(cx);
                let only_me = only_me.get();
                window.close_dialog(cx);
                let _ = this.update(cx, |w, cx| w.save_project_settings(&project, config, only_me, window, cx));
            }
        };
        let remove = {
            let (this, project) = (this.clone(), project.clone());
            move |window: &mut Window, cx: &mut App| {
                window.close_dialog(cx);
                let _ = this.update(cx, |w, cx| {
                    if let Some(pi) = w.projects.iter().position(|p| p.path == project) {
                        w.remove_project(pi, window, cx);
                    }
                });
            }
        };
        let ask = {
            let this = this.clone();
            move |window: &mut Window, cx: &mut App| {
                window.close_dialog(cx);
                let _ = this.update(cx, |w, cx| w.ask_agent_for_settings(pi, window, cx));
            }
        };
        let allow = {
            let (this, project) = (this.clone(), project.clone());
            move |key: String, allowed: bool, window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |w, cx| {
                    if let Some(p) = w.projects.iter_mut().find(|p| p.path == project) {
                        p.trusted.insert(key, allowed);
                    }
                    w.save_later(cx);
                });
                window.refresh();
            }
        };
        let file_trust = Rc::new(Cell::new(file_trust));
        let has_suggestions = !suggested.is_empty();
        window.open_dialog(cx, move |dialog, _, _| {
            let (setup, teardown, check, copy, rows, only_me) = (setup.clone(), teardown.clone(), check.clone(), copy.clone(), rows.clone(), only_me.clone());
            let (save, ask, fill, allow, file_trust, file_key, file_error) =
                (save.clone(), ask.clone(), fill.clone(), allow.clone(), file_trust.clone(), file_key.clone(), file_error.clone());
            let remove = remove.clone();
            dialog
                .title(format!("Project settings · {name}"))
                .w(px(760.))
                .content(move |content, window, cx| {
                    let muted = cx.theme().muted_foreground;
                    let height = (window.viewport_size().height - px(240.)).clamp(px(240.), px(640.));
                    let label = |title: &'static str, hint: &'static str| {
                        v_flex().w(px(150.)).flex_none().child(div().text_sm().child(title)).child(div().text_xs().text_color(muted).child(hint))
                    };
                    let field = |title: &'static str, hint: &'static str, input: AnyElement| h_flex().items_start().gap_3().child(label(title, hint)).child(div().flex_1().min_w_0().child(input));
                    let mine = only_me.get();
                    let services = {
                        let mut list = v_flex().gap_1();
                        for (ix, row) in rows.borrow().iter().enumerate() {
                            let rows = rows.clone();
                            list = list.child(
                                h_flex()
                                    .gap_1()
                                    .child(div().w(px(110.)).child(Input::new(&row.name).small()))
                                    .child(div().flex_1().min_w_0().child(Input::new(&row.command).small()))
                                    .child(div().w(px(70.)).child(Input::new(&row.port).small()))
                                    .child(Button::new(("service-remove", ix)).xsmall().ghost().label("×").tooltip("Remove").on_click(move |_, window, _| {
                                        if ix < rows.borrow().len() {
                                            rows.borrow_mut().remove(ix);
                                        }
                                        window.refresh();
                                    })),
                            );
                        }
                        let rows = rows.clone();
                        list.child(h_flex().child(Button::new("service-add").xsmall().ghost().label("+ Service").on_click(move |_, window, cx| {
                            let row = ServiceRow::empty(window, cx);
                            rows.borrow_mut().push(row);
                            window.refresh();
                        })))
                    };
                    let trust_row = match (file_key.clone(), file_trust.get()) {
                        (Some(key), state) if !mine => {
                            let (said, button, to) = match state {
                                Some(true) => ("The commands in .ideae.toml run in this project's tasks.", "Stop allowing", false),
                                Some(false) => ("You chose not to run the commands in .ideae.toml.", "Allow", true),
                                None => ("The commands in .ideae.toml have not run yet: you are asked first.", "Allow", true),
                            };
                            let (allow, file_trust) = (allow.clone(), file_trust.clone());
                            Some(
                                h_flex()
                                    .gap_2()
                                    .child(div().flex_1().text_xs().text_color(muted).child(said))
                                    .child(Button::new("trust-file").xsmall().outline().label(button).on_click(move |_, window, cx| {
                                        file_trust.set(Some(to));
                                        allow(key.clone(), to, window, cx);
                                    })),
                            )
                        }
                        _ => None,
                    };
                    content.child(
                        v_flex()
                            .id("project-settings")
                            .test_support()
                            .max_h(height)
                            .overflow_y_scroll()
                            .gap_3()
                            .pr_2()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(Checkbox::new("only-me").label("Only for me").checked(mine).on_click({
                                        let only_me = only_me.clone();
                                        move |checked, window, _| {
                                            only_me.set(*checked);
                                            window.refresh();
                                        }
                                    }))
                                    .child(div().flex_1().text_xs().text_color(muted).child(if mine {
                                        "Kept in the app, nothing written to the repository; used in place of .ideae.toml.".to_string()
                                    } else {
                                        format!("Written to {} at the repository root: commit it, and the team has the same.", crate::services::FILE)
                                    })),
                            )
                            .when_some(file_error.clone(), |form, e| form.child(div().text_xs().text_color(gpui_kit::red()).child(e)))
                            .children(trust_row)
                            .child(field("Setup", "Runs in each new task's worktree, next to the agent", Textarea::new(&setup).into_any_element()))
                            .child(field("Teardown", "Runs when a task is paused or archived", Textarea::new(&teardown).into_any_element()))
                            .child(field("Check", "Runs when the agent finishes a turn; ✓ or ✗ on the task", Input::new(&check).into_any_element()))
                            .child(field("Files to copy", "Ignored files a new worktree gets from the main checkout", Input::new(&copy).into_any_element()))
                            .child(field("Services", "Started from Run: name, command, port", services.into_any_element()))
                            .child({
                                let remove = remove.clone();
                                h_flex()
                                    .pt_3()
                                    .mt_1()
                                    .gap_2()
                                    .border_t_1()
                                    .border_color(cx.theme().border)
                                    .child(div().flex_1().text_xs().text_color(cx.theme().muted_foreground).child("The project leaves the list; its folder, tasks' worktrees and branches stay on disk."))
                                    .child(Button::new("project-remove").small().ghost().label("Remove from the list…").on_click(move |_, window, cx| remove(window, cx)))
                            }),
                    )
                })
                .footer({
                    let (save, ask, fill) = (save.clone(), ask.clone(), fill.clone());
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            Button::new("settings-suggest")
                                .ghost()
                                .label("Use suggestions")
                                .disabled(!has_suggestions)
                                .tooltip("Fill what is empty with what the project's files suggest (the grey text)")
                                .on_click(move |_, window, cx| fill(window, cx)),
                        )
                        .child(
                            Button::new("settings-ask-agent")
                                .ghost()
                                .label("Ask the agent to fill this in")
                                .tooltip("The agent of the main checkout writes .ideae.toml for this repository; you review it as any change")
                                .on_click(move |_, window, cx| ask(window, cx)),
                        )
                        .child(div().flex_1())
                        .child(Button::new("project-settings-cancel").ghost().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("project-settings-save").primary().label("Save").on_click(move |_, window, cx| save(window, cx)))
                })
        });
    }

    fn save_project_settings(&mut self, project: &Path, config: Config, only_me: bool, window: &mut Window, cx: &mut Context<Self>) {
        let project = project.to_path_buf();
        if only_me {
            project_config::set_own(&project, Some(config));
            self.save_later(cx);
            self.settings_changed(&project, cx);
            return;
        }
        project_config::set_own(&project, None);
        // What you wrote yourself is not asked about.
        if let (Some(key), Some(p)) = (project_config::trust_key(&config.task), self.projects.iter_mut().find(|p| p.path == project)) {
            p.trusted.insert(key, true);
        }
        self.save_later(cx);
        cx.spawn_in(window, async move |this, cx| {
            let job = project.clone();
            let written = cx.background_spawn(async move { project_config::write_file(&job, &config) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Err(e) = written {
                    window.push_notification(Notification::error(e).title("Project settings"), cx);
                }
                this.settings_changed(&project, cx);
            });
        })
        .detach();
    }

    /// The project's tasks read their settings again.
    fn settings_changed(&mut self, project: &Path, cx: &mut Context<Self>) {
        if let Some(p) = self.projects.iter().find(|p| p.path == project) {
            for view in p.tasks.iter().filter_map(|t| t.view.clone()) {
                view.update(cx, |t, cx| t.reread_settings(cx));
            }
        }
        cx.notify();
    }
}

/// One service in the settings: what is edited, and what is kept of it
/// (its address, variables, folder).
struct ServiceRow {
    name: Entity<InputState>,
    command: Entity<InputState>,
    port: Entity<InputState>,
    kept: Service,
}

impl ServiceRow {
    fn new_in(service: Service, window: &mut Window, cx: &mut App) -> Self {
        let make = |value: String, hint: &'static str, window: &mut Window, cx: &mut App| {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(hint));
            state.update(cx, |s, cx| s.set_value(value, window, cx));
            state
        };
        Self {
            name: make(service.name.clone(), "name", window, cx),
            command: make(service.command.clone(), "command, {port} for its port", window, cx),
            port: make(service.port.map(|p| p.to_string()).unwrap_or_default(), "port", window, cx),
            kept: service,
        }
    }

    fn empty(window: &mut Window, cx: &mut App) -> Self {
        Self::new_in(Service { name: String::new(), command: String::new(), port: None, url: None, env: Default::default(), cwd: None }, window, cx)
    }

    fn service(&self, cx: &App) -> Option<Service> {
        let name = self.name.read(cx).value().trim().to_string();
        let command = self.command.read(cx).value().trim().to_string();
        if name.is_empty() || command.is_empty() {
            return None;
        }
        let port = self.port.read(cx).value().trim().parse().ok();
        let url = self.kept.url.clone().or_else(|| (port.is_some() && self.kept.name.is_empty()).then(|| "http://localhost:{port}".to_string()));
        Some(Service { name, command, port, url, ..self.kept.clone() })
    }
}

/// The commands a file would run, as the trust question shows them.
fn commands_list(project: &str, task: &TaskSetup, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut list = v_flex().gap_2().child(div().text_sm().text_color(muted).child(format!(
        "Anyone who can commit to {project} can change these. They run with your rights, in each task's worktree; asked again when they change."
    )));
    let check = [task.check.trim().to_string()].into_iter().filter(|c| !c.is_empty()).collect::<Vec<_>>();
    for (what, commands) in [("Setup, in each new task", &task.setup), ("Teardown, when a task is paused or archived", &task.teardown), ("Check, when an agent finishes a turn", &check)] {
        if commands.is_empty() {
            continue;
        }
        list = list.child(
            v_flex()
                .child(div().text_xs().text_color(muted).child(what))
                .children(commands.iter().map(|c| div().font_family(cx.theme().mono_font_family.clone()).text_sm().child(format!("$ {c}")))),
        );
    }
    list.into_any_element()
}

const AGENT_PROMPT: &str = "Write .ideae.toml at the root of this repository, or update the one there, for ideae (the app running you), which reads it for every task (a git worktree with its own agent):

[task]
setup = [\"…\"]     # commands that make a fresh worktree ready to work in: install dependencies, generate code
teardown = [\"…\"]  # what stops or removes what a task started (containers, databases); often nothing
check = \"…\"       # one command that tells whether the work holds: tests, a type check; run after every turn of an agent, so not too slow
copy = [\".env\"]   # ignored files a worktree needs from the main checkout, as globs from the root

[[service]]        # dev servers and workers, one each
name = \"web\"
command = \"npm run dev -- --port {port}\"  # {port} is `port` + 100 × the task's slot, so tasks run side by side
port = 3000
url = \"http://localhost:{port}\"

Read the README, the build files and the CI configuration to choose. Leave out what does not apply, keep what is there already, and do not commit: I will review it.";

#[cfg(all(test, unix))]
mod tests {
    use crate::state::{ProjectState, State};
    use crate::workspace::Workspace;
    use gpui_kit::{AppContext as _, Bounds, Point, TestAppContext, WindowBounds, WindowOptions, px, size};
    use std::time::Duration;

    /// A task of a repository whose `.ideae.toml` sets it up, checks it and
    /// tears it down: setup in a terminal, the check after each turn, the
    /// teardown before the worktree goes.
    #[gpui_kit::test]
    #[ignore]
    fn a_task_is_set_up_checked_and_torn_down(cx: &mut TestAppContext) {
        use crate::agent::HookEvent;
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        cx.executor().allow_parking();
        let dir = std::env::temp_dir().join(format!("ideae-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let torn = dir.join("torn.txt");
        // The setup also leaves a process in a session of its own: no terminal
        // ends it, the task's end has to find it.
        let stray = if cfg!(target_os = "linux") { r#", "setsid sh -c 'echo $$ > stray.pid; exec sleep 300' </dev/null >/dev/null 2>&1 &""# } else { "" };
        let toml = format!(
            "[task]\nsetup = [\"echo made > setup.txt\"{stray}]\nteardown = [\"pwd > '{}'\"]\ncheck = \"test -f ok.txt || (echo no ok.txt; exit 1)\"\ncopy = [\".env\"]\n",
            torn.display()
        );
        std::fs::write(dir.join("repo/.ideae.toml"), &toml).unwrap();
        let script = "cd repo; git init -q -b main; git config user.email t@t; git config user.name t; echo .env > .gitignore; echo A=1 > .env; \
            git add .gitignore .ideae.toml; git commit -qm base";
        assert!(std::process::Command::new("sh").arg("-ec").arg(script).current_dir(&dir).status().unwrap().success());
        let repo = dir.join("repo").canonicalize().unwrap();
        // SAFETY: set before anything of the app reads them.
        unsafe {
            std::env::set_var("IDEAE_STATE", dir.join("state.json"));
            std::env::set_var("IDEAE_NO_DAEMON", "1");
        }
        // The file's commands were allowed before.
        let key = crate::project_config::trust_key(&crate::project_config::parse(&toml).unwrap().task).unwrap();
        let project = ProjectState { path: repo.clone(), trusted: [(key, true)].into(), ..Default::default() };
        let saved = State { projects: vec![project], ..Default::default() };
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
        let step = |cx: &mut TestAppContext, act: &mut dyn FnMut(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>)| {
            cx.update_window(handle.into(), |_, window, cx| workspace.update(cx, |w, cx| act(w, window, cx))).unwrap();
            cx.run_until_parked();
        };
        // Terminals, checks and teardowns run on threads of their own; the
        // setup's terminal is looked at every second of the app's clock.
        let until = |cx: &mut TestAppContext, what: &str, done: &mut dyn FnMut(&mut TestAppContext) -> bool| {
            for _ in 0..1000 {
                if done(cx) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
                cx.executor().advance_clock(Duration::from_millis(250));
                cx.run_until_parked();
            }
            panic!("waited in vain for {what}");
        };
        let mark = |cx: &mut TestAppContext| {
            workspace.update(cx, |w, cx| {
                let marks = w.projects[0].tasks.iter().find(|t| !t.main).and_then(|t| Some(t.view.as_ref()?.read(cx).auto_marks())).unwrap_or_default();
                marks.iter().map(|m| m.0).collect::<Vec<_>>().join(" ")
            })
        };

        step(cx, &mut |w, window, cx| w.create_task(0, "Fix login".into(), String::new(), vec![String::new()], Vec::new(), window, cx));
        let mut path = None;
        until(cx, "the worktree", &mut |cx| {
            path = workspace.update(cx, |w, _| w.projects[0].tasks.iter().find(|t| !t.main && t.busy.is_none() && t.view.is_some()).map(|t| t.path.clone()));
            path.is_some()
        });
        let path = path.unwrap();
        assert_eq!(std::fs::read_to_string(path.join(".env")).unwrap(), "A=1\n", "the ignored file it names is copied");
        until(cx, "the setup", &mut |cx| path.join("setup.txt").exists() && mark(cx).is_empty());
        let stray_pid = cfg!(target_os = "linux").then(|| {
            let file = path.join("stray.pid");
            let mut pid = None;
            until(cx, "the stray to start", &mut |_| {
                pid = std::fs::read_to_string(&file).ok().and_then(|p| p.trim().parse::<i32>().ok());
                pid.is_some()
            });
            pid.unwrap()
        });

        let hook = |event: &str| HookEvent { task: path.clone(), event: event.into(), message: None, kind: None, file: None, said: None, replayed: None, session: None, guessed: false };
        let turn = |cx: &mut TestAppContext| {
            step(cx, &mut |w, window, cx| w.on_hook(hook("UserPromptSubmit"), window, cx));
            step(cx, &mut |w, window, cx| w.on_hook(hook("Stop"), window, cx));
        };
        turn(cx);
        until(cx, "the check to fail", &mut |cx| mark(cx) == "✗ check");
        std::fs::write(path.join("ok.txt"), "").unwrap();
        turn(cx);
        until(cx, "the check to pass", &mut |cx| mark(cx) == "✓ check");

        step(cx, &mut |w, window, cx| w.archive_task(&path, window, cx));
        until(cx, "the task to go", &mut |cx| workspace.update(cx, |w, _| w.projects[0].tasks.iter().all(|t| t.main)));
        assert!(!path.exists(), "the worktree is gone");
        if let Some(pid) = stray_pid {
            // SAFETY: kill with signal 0 only asks whether it is there.
            let alive = || unsafe { libc::kill(pid, 0) } == 0 && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "));
            until(cx, "what the task left behind to end", &mut |_| !alive());
        }
        let torn_in = std::fs::read_to_string(&torn).expect("the teardown ran");
        assert_eq!(std::path::Path::new(torn_in.trim()), path, "in the worktree, before it went");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
