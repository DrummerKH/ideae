//! Questions asked aside: an agent of their own in a task's folder, beside
//! the task's agent, which goes on with its work. Each is `q-<n>` in what
//! its agent reports, a row under the task, a tab of the task. Asked of a
//! project, they go to its main thread.
//!
//! The task's folder then holds sessions of more than one agent: none of
//! them is resumed as "the latest in the folder" any more, each by its id.

use super::*;
use gpui_kit::component::ActiveTheme as _;
use crate::agents::{Launch, Start};
use crate::task::{Asked, Who};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};

/// How many closed questions a task keeps, the latest.
const CLOSED_KEPT: usize = 50;
/// How much of an answer its row can show.
const ANSWER_KEPT: usize = 1200;

/// What is done once a question is asked: with its task and its number,
/// none when it could not be (no agent to ask).
pub(super) type AskedThen = Box<dyn FnOnce(&mut Workspace, &Path, Option<u32>, &mut Context<Workspace>)>;

/// An agent of the project a question can start from.
#[derive(Clone)]
struct QuestionSource {
    label: String,
    profile: String,
    session: String,
    root: PathBuf,
}

/// What a question was given of that agent's conversation.
enum Given {
    Nothing,
    /// Its agent starts from it; `copies` were put where it looks for it.
    Fork { from: String, copies: Vec<PathBuf> },
    /// A file it can read.
    Read(PathBuf),
}

/// Not for the UI thread: it reads and copies the conversation's files.
fn give_context(source: &QuestionSource, asked: Option<&str>, to: &Path) -> Given {
    let Some(profile) = state::with_settings(|s| s.profile(&source.profile).cloned()) else { return Given::Nothing };
    if asked == Some(source.profile.as_str()) && profile.adapter().forks() {
        let copies = if source.root == to { Vec::new() } else { carry_session(&profile, &source.root, to, Some(&source.session), None) };
        return Given::Fork { from: source.session.clone(), copies };
    }
    let who = crate::agents::SessionRef { profile, id: Some(source.session.clone()) };
    who.own_transcript(&source.root).map_or(Given::Nothing, Given::Read)
}

impl Workspace {
    /// The agents of a project with a conversation to start a question from.
    fn question_sources(&self, pi: usize) -> Vec<QuestionSource> {
        let Some(p) = self.projects.get(pi) else { return Vec::new() };
        let usable = |id: &str| state::with_settings(|s| s.profile(id).is_some_and(|profile| !profile.acp && (profile.adapter().forks() || profile.adapter().session_base(profile, &p.path).is_some())));
        let mut sources = Vec::new();
        for t in p.tasks.iter().filter(|t| t.busy.is_none()) {
            if let Some(who) = t.session_ref().filter(|who| usable(&who.profile.id)) {
                if let Some(session) = who.id {
                    let label = if t.main { format!("{} · main thread", who.profile.name) } else { format!("{} · {}", who.profile.name, t.label()) };
                    sources.push(QuestionSource { label, profile: who.profile.id, session, root: t.path.clone() });
                }
            }
            for q in t.questions.iter().filter(|q| q.closed == 0 && usable(&q.profile)) {
                if let Some(session) = &q.session {
                    sources.push(QuestionSource { label: format!("? {}", q.title), profile: q.profile.clone(), session: session.clone(), root: t.path.clone() });
                }
            }
        }
        sources
    }

    /// Asks what the question is, and of which agent when there is a choice.
    pub fn ask_question_dialog(&mut self, pi: usize, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::input::{Textarea, TextareaState};
        let Some(p) = self.projects.get(pi) else { return };
        let name = p.name.clone();
        let prompt = cx.new(|cx| TextareaState::new(window, cx).auto_grow(4, 14).placeholder("What do you want to know? It is answered beside the main thread's agent, which goes on with its work."));
        let agent = Rc::new(RefCell::new(self.project_agent(pi)));
        let sources = Rc::new(self.question_sources(pi));
        let source: Rc<RefCell<Option<QuestionSource>>> = Rc::default();
        let this = cx.weak_entity();
        let submit = {
            let (prompt, agent, source, this) = (prompt.clone(), agent.clone(), source.clone(), this.clone());
            move |window: &mut Window, cx: &mut App| {
                let text = prompt.read(cx).value().trim().to_string();
                if text.is_empty() {
                    return;
                }
                window.close_dialog(cx);
                let (profile, source) = (agent.borrow().clone(), source.borrow().clone());
                let _ = this.update(cx, |this, cx| this.ask(pi, text, profile, source, None, window, cx));
            }
        };
        // Runs inside this entity's update, so it cannot go through `submit`.
        let on_enter = {
            let (agent, source) = (agent.clone(), source.clone());
            move |this: &mut Self, prompt: &Entity<TextareaState>, event: &InputEvent, window: &mut Window, cx: &mut Context<Self>| {
                let InputEvent::PressEnter { secondary: true, .. } = event else { return };
                let text = prompt.read(cx).value().trim().to_string();
                if !text.is_empty() {
                    window.close_dialog(cx);
                    let (profile, source) = (agent.borrow().clone(), source.borrow().clone());
                    this.ask(pi, text, profile, source, None, window, cx);
                }
            }
        };
        let listening = Rc::new(cx.subscribe_in(&prompt, window, on_enter));
        let focused = Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, cx| {
            let (prompt, submit, agent, focused, source, sources) = (prompt.clone(), submit.clone(), agent.clone(), focused.clone(), source.clone(), sources.clone());
            let _ = &listening;
            let muted = cx.theme().muted_foreground;
            let several = state::with_settings(|s| s.several_agents());
            dialog
                .title(format!("A question about {name}"))
                .w(px(560.))
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let prompt = prompt.clone();
                        window.defer(cx, move |window, cx| prompt.read(cx).focus_handle(cx).focus(window, cx));
                    }
                    content.child(
                        v_flex()
                            .gap_2()
                            .child(Textarea::new(&prompt))
                            .when(!sources.is_empty(), |form| {
                                form.child(h_flex().gap_2().items_center().child(div().text_sm().child("Context")).child(context_picker(sources.clone(), source.clone())))
                            })
                            .when(several, |form| {
                                form.child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(div().text_sm().child("Agent"))
                                        .child(agents_ui::picker("question-agent", agent.clone())),
                                )
                            }),
                    )
                })
                .footer({
                    let submit = submit.clone();
                    h_flex()
                        .justify_end()
                        .items_center()
                        .gap_2()
                        .child(div().text_xs().text_color(muted).child(crate::keys::label("Ctrl+Enter")))
                        .child(Button::new("ask-question").primary().label("Ask").on_click(move |_, window, cx| submit(window, cx)))
                })
        });
    }

    /// A question asked by a task's agent (crew.rs), in the task's project,
    /// from that agent's conversation when `from` names its task; `then`
    /// has the main thread and the question's number once it is asked.
    pub(super) fn ask_for_agent(&mut self, pi: usize, prompt: String, from: Option<&Path>, then: AskedThen, window: &mut Window, cx: &mut Context<Self>) {
        let source = from.and_then(|from| self.question_sources(pi).into_iter().find(|s| s.root == from));
        self.ask(pi, prompt, String::new(), source, Some(then), window, cx);
    }

    /// A question of the project's main thread: its agent starts there (the
    /// main thread's own only when it is opened), told `prompt`.
    fn ask(&mut self, pi: usize, prompt: String, profile: String, source: Option<QuestionSource>, then: Option<AskedThen>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(pi) else { return };
        let path = p.path.clone();
        let Some(ti) = p.tasks.iter().position(|t| t.main) else {
            self.open_main(pi, window, cx);
            return self.ask(pi, prompt, profile, source, then, window, cx);
        };
        let entry = &p.tasks[ti];
        // The first question: the main thread's own session, when its id is
        // not known, is found now, while it is the latest in the folder.
        let unpinned = (entry.asked == 0).then(|| entry.session_ref()).flatten().filter(|who| who.id.is_none());
        let root = path.clone();
        let settings = state::settings();
        let asked = crate::agents::resolve(&settings.agents, &[&profile], Some(&p.values().agent), &settings.default_agent).map(|profile| profile.id.clone());
        let to = path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let pinned = match unpinned {
                Some(who) => cx.background_spawn(async move { who.transcript(&root).and_then(|file| file.file_stem().map(|s| s.to_string_lossy().into_owned())) }).await,
                None => None,
            };
            let given = match source {
                Some(source) => cx.background_spawn(async move { give_context(&source, asked.as_deref(), &to) }).await,
                None => Given::Nothing,
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if let (Some(id), Some(entry)) = (pinned, this.entry_mut(&path)) {
                    if let Some(session) = entry.sessions.last_mut().filter(|s| s.id.is_none()) {
                        session.id = Some(id.clone());
                    }
                    if let Some(view) = entry.view.clone() {
                        view.update(cx, |t, _| t.agent.learned_session(&id));
                    }
                }
                let asked = this.ask_now(pi, &path, prompt, profile, given, then.is_some(), window, cx);
                if let Some(then) = then {
                    then(this, &path, asked, cx);
                }
            });
        })
        .detach();
    }

    /// The question's number, once asked.
    /// `quiet`: asked by an agent, the window stays on what you look at.
    fn ask_now(&mut self, pi: usize, path: &Path, prompt: String, profile: String, given: Given, quiet: bool, window: &mut Window, cx: &mut Context<Self>) -> Option<u32> {
        let settings = state::settings();
        let project_agent = self.projects[pi].values().agent.clone();
        let Some(profile) = crate::agents::resolve(&settings.agents, &[&profile], Some(&project_agent), &settings.default_agent).cloned() else {
            window.push_notification(Notification::warning("No agent to ask: add one in Settings → Agents."), cx);
            return None;
        };
        let id = profile.adapter().chooses_session_id().then(crate::agents::uuid);
        // Loaded before the question is listed, which it would take for one of before.
        if quiet {
            self.quiet_start.insert(path.to_path_buf());
            self.load(path, window, cx);
            self.quiet_start.remove(path);
        } else {
            self.switch_quietly(path, window, cx);
        }
        let entry = self.entry_mut(path)?;
        entry.asked += 1;
        let n = entry.asked;
        let title = first_line(&prompt);
        entry.questions.push(state::QuestionState { id: n, title: title.clone(), profile: profile.id.clone(), session: id.clone(), created: state::now(), ..Default::default() });
        let (start, told) = match given {
            Given::Nothing => (Start::New(id), prompt.clone()),
            Given::Fork { from, copies } => {
                if !copies.is_empty() {
                    self.fork_copies.insert((path.to_path_buf(), n), copies);
                }
                (Start::Fork { from, id }, prompt.clone())
            }
            Given::Read(file) => (Start::New(id), format!("Another agent of this project has a conversation in {}. Read what you need of it for this question.\n\n{prompt}", file.display())),
        };
        let launch = Launch { profile, start, unasked: false, aside: true, options: Vec::new(), host: crate::hosts::split(path).map(|(host, _)| host) };
        if let Some(view) = self.entry_mut(path).and_then(|e| e.view.clone()) {
            view.update(cx, |t, cx| {
                t.asked = true;
                t.ask(n, title.clone().into(), launch, Some(told), window, cx);
            });
        }
        self.sync_worktrees(window, cx);
        self.save_later(cx);
        cx.notify();
        // More than its first line says: the agent for quick questions names it.
        if prompt_says_more(&prompt, &title) {
            let (path, first) = (path.to_path_buf(), title.clone());
            cx.spawn_in(window, async move |this, cx| {
                let Some(name) = cx.background_spawn(crate::procs::on_thread(move || name_question(&prompt))).await else { return };
                let _ = this.update(cx, |this, cx| {
                    // Not over a name you gave it meanwhile.
                    let renamed = this.entry_mut(&path).and_then(|e| e.questions.iter().find(|q| q.id == n)).is_none_or(|q| q.title != first);
                    if !renamed {
                        this.rename_question(&path, n, name, cx);
                    }
                });
            })
            .detach();
        }
        Some(n)
    }

    /// A name of your own for a question.
    pub fn rename_question_dialog(&mut self, path: &Path, n: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.entry_mut(path).and_then(|e| e.questions.iter().find(|q| q.id == n)).map(|q| q.title.clone()) else { return };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Question"));
        input.update(cx, |i, cx| {
            i.set_value(current, window, cx);
            i.select_all(window, cx);
        });
        let (path, this) = (path.to_path_buf(), cx.weak_entity());
        let focused = Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, path, this, focused) = (input.clone(), path.clone(), this.clone(), focused.clone());
            let apply = {
                let (input, path, this) = (input.clone(), path.clone(), this.clone());
                move |window: &mut Window, cx: &mut App| {
                    let title = input.read(cx).value().trim().to_string();
                    if title.is_empty() {
                        return;
                    }
                    window.close_dialog(cx);
                    let _ = this.update(cx, |this, cx| this.rename_question(&path, n, title, cx));
                }
            };
            dialog
                .title("Rename question")
                .w(px(480.))
                // Enter in the input reaches the dialog.
                .on_ok({
                    let apply = apply.clone();
                    move |_, window, cx| {
                        apply(window, cx);
                        false
                    }
                })
                .content(move |content, window, cx| {
                    if !focused.replace(true) {
                        let input = input.clone();
                        window.defer(cx, move |window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
                    }
                    content.child(Input::new(&input))
                })
                .footer(h_flex().justify_end().child(Button::new("rename-question").primary().label("Rename").on_click(move |_, window, cx| apply(window, cx))))
        });
    }

    pub fn rename_question(&mut self, path: &Path, n: u32, title: String, cx: &mut Context<Self>) {
        let Some(entry) = self.entry_mut(path) else { return };
        let Some(q) = entry.questions.iter_mut().find(|q| q.id == n) else { return };
        q.title = title.clone();
        if let Some(view) = entry.view.clone() {
            view.update(cx, |t, cx| t.rename_question(n, title.into(), cx));
        }
        self.save_later(cx);
        cx.notify();
    }

    /// The open questions of a task just loaded, listed in it; their agents
    /// start when they are shown.
    pub(super) fn give_questions(&self, entry: &TaskEntry, view: &mut crate::task::TaskView) {
        view.asked = entry.asked > 0;
        for q in entry.questions.iter().filter(|q| q.closed == 0) {
            if let Some(launch) = question_launch(q) {
                view.add_question(q.id, q.title.clone().into(), launch);
                if q.asleep {
                    view.question_sleeps(q.id);
                }
                if q.active > 0 {
                    view.question_talked(q.id);
                }
            }
        }
    }

    /// The task in view, its own agent not started if it was not running:
    /// a question was asked of it, or one of its questions is wanted.
    fn switch_quietly(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.entry_mut(path).is_some_and(|e| e.view.is_none()) {
            self.quiet_start.insert(path.to_path_buf());
        }
        self.switch(path, window, cx);
        self.quiet_start.remove(path);
    }

    /// Questions left alone: their agents stop when idle a while out of
    /// sight; open ones nobody came back to for days are closed.
    /// IDEAE_QUESTION_IDLE_SECS shortens the first for tests.
    pub(super) fn tend_questions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (sleep, close) = state::with_settings(|s| (s.question_sleep_minutes, s.question_close_days));
        let sleep = std::env::var("IDEAE_QUESTION_IDLE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(u64::from(sleep) * 60);
        if sleep > 0 {
            for view in self.views() {
                if !view.read(cx).questions.is_empty() {
                    view.update(cx, |t, cx| t.sleep_idle_questions(std::time::Duration::from_secs(sleep), cx));
                }
            }
        }
        if close == 0 {
            return;
        }
        let before = state::now().saturating_sub(u64::from(close) * 24 * 3600);
        let old: Vec<(PathBuf, u32)> = self
            .projects
            .iter()
            .flat_map(|p| &p.tasks)
            .flat_map(|t| {
                // Not one at work or asking, whatever its age.
                let busy = |n: u32| t.view.as_ref().and_then(|v| v.read(cx).agent_of(Who::Question(n)).map(|a| matches!(a.status, agent::AgentStatus::Working | agent::AgentStatus::NeedsInput(_)))).unwrap_or(false);
                t.questions.iter().filter(move |q| q.closed == 0 && !q.unread && q.active.max(q.created) < before).filter(move |q| !busy(q.id)).map(move |q| (t.path.clone(), q.id))
            })
            .collect();
        for (path, n) in old {
            self.close_question(&path, n, window, cx);
        }
    }

    /// Marks a question's last message now, to the minute.
    fn touch_question(&mut self, path: &Path, n: u32) {
        let now = state::now();
        if let Some(q) = self.entry_mut(path).and_then(|e| e.questions.iter_mut().find(|q| q.id == n)).filter(|q| now.saturating_sub(q.active) >= 60) {
            q.active = now;
        }
    }

    /// The pointer on a row with an answer: left there a moment, it was read.
    pub(crate) fn hover_answer(&mut self, path: &Path, n: u32, on: bool, cx: &mut Context<Self>) {
        if !on {
            self.answer_hover = Task::ready(());
            return;
        }
        let path = path.to_path_buf();
        self.answer_hover = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(q) = this.entry_mut(&path).and_then(|e| e.questions.iter_mut().find(|q| q.id == n)).filter(|q| q.unread && !q.answer.is_empty()) {
                    q.unread = false;
                    this.save_later(cx);
                    cx.notify();
                }
            });
        });
    }

    /// A question's row clicked: its task in view, its tab in front.
    pub fn show_question(&mut self, path: &Path, n: u32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(q) = self.entry_mut(path).and_then(|e| e.questions.iter_mut().find(|q| q.id == n)) {
            q.asleep = false;
            q.opened = state::now();
        }
        self.switch_quietly(path, window, cx);
        if let Some(view) = self.entry_mut(path).and_then(|e| e.view.clone()) {
            view.update(cx, |t, cx| t.show_question(n, window, cx));
        }
    }

    /// Its agent ends; the question is kept among the task's closed ones.
    pub fn close_question(&mut self, path: &Path, n: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entry_mut(path) else { return };
        if let Some(q) = entry.questions.iter_mut().find(|q| q.id == n) {
            q.closed = state::now();
        }
        let closed = entry.questions.iter().filter(|q| q.closed > 0).count();
        if closed > CLOSED_KEPT {
            // The first asked go first.
            let mut over = closed - CLOSED_KEPT;
            entry.questions.retain(|q| {
                let goes = q.closed > 0 && over > 0;
                over -= usize::from(goes);
                !goes
            });
        }
        if let Some(view) = entry.view.clone() {
            view.update(cx, |t, cx| t.end_question(n, window, cx));
        }
        self.save_later(cx);
        cx.notify();
    }

    /// The main thread's closed questions, the latest first: one picked is
    /// asked again, its conversation going on.
    pub fn closed_questions_dialog(&mut self, pi: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(pi) else { return };
        let Some(main) = p.tasks.iter().find(|t| t.main) else { return };
        let path = main.path.clone();
        let mut closed: Vec<(u32, String, u64)> = main.questions.iter().filter(|q| q.closed > 0).map(|q| (q.id, q.title.clone(), q.closed)).collect();
        closed.sort_by_key(|(_, _, at)| std::cmp::Reverse(*at));
        let name = p.name.clone();
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (closed, path, this) = (closed.clone(), path.clone(), this.clone());
            dialog.title(format!("Closed questions of {name}")).w(px(560.)).content(move |content, _, cx| {
                let muted = cx.theme().muted_foreground;
                if closed.is_empty() {
                    return content.child(div().text_sm().text_color(muted).child("None yet: a question closed from its bar or its row comes here."));
                }
                let rows = closed.iter().map(|(n, title, at)| {
                    let (n, path, this) = (*n, path.clone(), this.clone());
                    h_flex()
                        .id(SharedString::from(format!("closed-question-{n}")))
                        .px_2()
                        .py_1()
                        .gap_2()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().muted))
                        .child(div().flex_1().min_w_0().truncate().text_sm().child(title.clone()))
                        .child(div().flex_none().text_xs().text_color(muted).child(crate::sidebar::ago(state::now().saturating_sub(*at))))
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let _ = this.update(cx, |this, cx| this.reopen_question(&path, n, window, cx));
                        })
                });
                content.child(v_flex().id("closed-questions").gap_0p5().max_h(px(420.)).overflow_y_scroll().children(rows))
            })
        });
    }

    /// A closed question asked again: its conversation goes on.
    pub fn reopen_question(&mut self, path: &Path, n: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entry_mut(path) else { return };
        let Some(q) = entry.questions.iter_mut().find(|q| q.id == n) else { return };
        q.closed = 0;
        q.asleep = false;
        let (title, launch) = (q.title.clone(), question_launch(q));
        self.switch_quietly(path, window, cx);
        if let (Some(view), Some(launch)) = (self.entry_mut(path).and_then(|e| e.view.clone()), launch) {
            view.update(cx, |t, cx| {
                t.add_question(n, title.into(), launch);
                t.show_question(n, window, cx);
            });
        }
        self.save_later(cx);
        cx.notify();
    }

    /// The question goes on as a task of its own: a worktree and a branch
    /// named after it, its agent's session carried there and resumed, told
    /// to move what it wrote. Its agent in the main thread ends first, not
    /// to write its session while it is copied.
    pub fn question_to_task(&mut self, path: &Path, n: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, ti)) = self.find(path) else { return };
        let Some(q) = self.projects[pi].tasks[ti].questions.iter().find(|q| q.id == n).cloned() else { return };
        let Some(profile) = state::with_settings(|s| s.profile(&q.profile).cloned()) else { return };
        let from_branch = self.projects[pi].tasks[ti].branch.clone();
        if let Some(view) = self.projects[pi].tasks[ti].view.clone() {
            view.update(cx, |t, cx| t.end_question(n, window, cx));
        }
        self.projects[pi].tasks[ti].questions.retain(|q| q.id != n);
        let p = &mut self.projects[pi];
        let branch = naming::branch_name_as(&q.title, &p.values().branch);
        let (repo, base) = (p.path.clone(), p.values().base.clone());
        let task = git::task_worktree_dir(&repo, &branch);
        p.collapsed = false;
        p.tasks.push(TaskEntry {
            path: task.clone(),
            branch: branch.clone(),
            title: q.title.clone(),
            last_active: state::now(),
            busy: Some("creating…".into()),
            prompt: q.title.clone(),
            created: state::now(),
            agent: profile.id.clone(),
            // Its session, resumed; none known, a new one told what was asked.
            sessions: q.session.iter().map(|id| state::AgentSession { profile: profile.id.clone(), id: Some(id.clone()), started: q.created }).collect(),
            fresh: q.session.is_none(),
            ..Default::default()
        });
        self.save_later(cx);
        cx.notify();
        let from = path.to_path_buf();
        let new_branch = branch.clone();
        cx.spawn_in(window, async move |this, cx| {
            let copy_env = state::settings().copy_env;
            let (task_dir, from_dir, session, tag) = (task.clone(), from.clone(), q.session.clone(), Who::Question(n).tag());
            let made = cx
                .background_spawn(async move {
                    let copy = crate::project_config::resolve(&repo).config.task.copy;
                    let made = git::create_task_worktree(&repo, &branch, &base, copy_env && copy.is_none());
                    if let (Ok(dir), Some(copy)) = (&made, copy) {
                        crate::project_config::copy_ignored(&repo, dir, &copy);
                    }
                    if made.is_ok() {
                        carry_session(&profile, &from_dir, &task_dir, session.as_deref(), tag.as_deref());
                    }
                    made
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match made {
                    Ok(_) => {
                        if let Some(entry) = this.entry_mut(&task) {
                            entry.busy = None;
                        }
                        this.switch(&task, window, cx);
                        this.task_made(&task, window, cx);
                        let told = moved_here(&q, &from, &from_branch, &task, &new_branch);
                        if let Some(view) = this.entry_mut(&task).and_then(|e| e.view.clone()) {
                            view.update(cx, |t, cx| t.queue_prompt(told, true, window, cx));
                        }
                    }
                    Err(e) => {
                        if let Some((pi, ti)) = this.find(&task) {
                            this.projects[pi].tasks.remove(ti);
                        }
                        // Not lost: back among the main thread's closed questions.
                        if let Some(entry) = this.entry_mut(&from) {
                            entry.questions.push(state::QuestionState { closed: state::now(), ..q.clone() });
                        }
                        window.push_notification(Notification::error(e).title("Could not make the task"), cx);
                    }
                }
                this.save_later(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn on_asked(&mut self, view: &Entity<crate::task::TaskView>, asked: &Asked, window: &mut Window, cx: &mut Context<Self>) {
        let root = view.read(cx).root.clone();
        match asked {
            Asked::Closed(n) => self.close_question(&root, *n, window, cx),
            Asked::ToTask(n) => self.question_to_task(&root, *n, window, cx),
            Asked::Seen(n) => {
                if let Some(q) = self.entry_mut(&root).and_then(|e| e.questions.iter_mut().find(|q| q.id == *n)) {
                    q.unread = false;
                    q.opened = state::now();
                    self.save_later(cx);
                    cx.notify();
                }
            }
        }
    }

    /// What a question's agent reported: its status, what it said, its
    /// session. The task's own agent and what follows its turns (the
    /// project's check, variants, usage) are left alone.
    pub(super) fn question_heard(&mut self, n: u32, event: agent::HookEvent, window: &mut Window, cx: &mut Context<Self>) {
        let who = Who::Question(n);
        let Some((pi, ti)) = self.find(&event.task) else { return };
        // It has read what it started from: the copy is not left among the folder's sessions.
        if let Some(files) = self.fork_copies.remove(&(event.task.clone(), n)) {
            cx.background_spawn(async move {
                for file in files {
                    let _ = crate::hostfs::remove(&file);
                }
            })
            .detach();
        }
        if let Some(made) = event.worktree.as_deref().map(PathBuf::from).filter(|made| *made != event.task) {
            self.projects[pi].made_by_agents.insert(made, event.task.clone());
        }
        let label = format!("{} · ? {}", self.projects[pi].name, self.projects[pi].tasks[ti].questions.iter().find(|q| q.id == n).map(|q| q.title.as_str()).unwrap_or_default());
        // A message, yours or its answer: what keeps an open question open.
        if matches!(event.event.as_str(), "UserPromptSubmit" | "Stop") && event.replayed.is_none() {
            self.touch_question(&event.task.clone(), n);
        }
        let entry = &mut self.projects[pi].tasks[ti];
        // What it writes, for its row in Changes and for taking along.
        if let (Some(file), Some(view)) = (event.file.as_deref().filter(|_| event.event == "PreToolUse"), entry.view.clone()) {
            let rel = view.update(cx, |t, cx| {
                let rel = t.mark_written(n, file);
                cx.notify();
                rel
            });
            let question = entry.questions.iter_mut().find(|q| q.id == n);
            if let (Some(rel), Some(q)) = (rel, question) {
                if !q.wrote.contains(&rel) && q.wrote.len() < 200 {
                    q.wrote.push(rel);
                }
            }
        }
        if let Some(id) = event.session.as_deref().filter(|id| !id.is_empty()) {
            if let Some(q) = entry.questions.iter_mut().find(|q| q.id == n).filter(|q| q.session.as_deref() != Some(id)) {
                q.session = Some(id.to_string());
            }
            if let Some(view) = entry.view.clone() {
                view.update(cx, |t, _| {
                    if let Some(agent) = t.agent_of_mut(who) {
                        agent.learned_session(id);
                    }
                });
            }
        }
        let Some(status) = event.status() else { return };
        let Some(view) = entry.view.clone() else { return };
        let Some(was) = view.read(cx).agent_of(who).map(|a| a.status.clone()) else { return };
        let status = if agent::asked_again(&was, &event) { was.clone() } else { status };
        let turn_over = status == agent::AgentStatus::Done && matches!(was, agent::AgentStatus::Working | agent::AgentStatus::NeedsInput(_));
        let seen = self.active.as_deref() == Some(event.task.as_path()) && window.is_window_active();
        // An agent's question: its answer goes to that agent; you are told only when it asks you.
        let for_agent = self.entry_mut(&event.task).and_then(|e| e.questions.iter().find(|q| q.id == n)).is_some_and(|q| q.asked_by.is_some());
        if turn_over && event.replayed.is_none() && !for_agent && !(seen && view.read(cx).questions_in_front().contains(&n)) {
            if let Some(q) = self.entry_mut(&event.task).and_then(|e| e.questions.iter_mut().find(|q| q.id == n)) {
                q.unread = true;
            }
            self.save_later(cx);
        }
        view.update(cx, |t, cx| {
            if let Some(agent) = t.agent_of_mut(who) {
                agent.hear(&event, &status);
                agent.guessed = event.guessed;
                match event.event.as_str() {
                    "PreToolUse" if status == agent::AgentStatus::Working => agent.doing = event.message.clone(),
                    "UserPromptSubmit" => agent.doing = Some("Thinking".into()),
                    _ => {}
                }
            }
            t.set_agent_status(who, status.clone(), seen, cx);
            if turn_over && event.replayed.is_none() {
                t.deliver_queued(who, window, cx);
            }
        });
        // Told on the desktop only with the window in the back: in front, its row says it.
        let attention = if for_agent { matches!(status, agent::AgentStatus::NeedsInput(_)) } else { status.is_attention() };
        if attention && status != was && !seen && !window.is_window_active() && event.replayed.is_none() && state::settings().notifications && self.shows(pi, ti) {
            let said = view.read(cx).agent_of(who).and_then(|a| a.shown_said().map(|s| s.whole.clone()));
            let words = state::with_settings(|s| s.notify_said && s.show_said) && !self.is_presenting();
            let body = crate::said::notice(&status, said.as_deref(), words);
            let (tx, path) = (self.open_task_tx.clone(), event.task.clone());
            crate::desktop::notify_with(&label, &body, move || {
                let _ = tx.send_blocking((path, Some(n)));
            });
        }
        if turn_over && event.replayed.is_none() {
            let said = view.read(cx).agent_of(who).and_then(|a| a.shown_said().map(|said| said.whole.to_string()));
            if let (Some(said), Some(q)) = (said, self.entry_mut(&event.task).and_then(|e| e.questions.iter_mut().find(|q| q.id == n))) {
                q.answer = said.chars().take(ANSWER_KEPT).collect();
            }
        }
        if event.event == "UserPromptSubmit" && event.replayed.is_none() {
            view.update(cx, |t, _| t.forget_written(n));
        }
        if status != was || turn_over {
            cx.notify();
        }
    }
}

/// A question's session where its agent looks for it in the task's
/// worktree: Claude Code keeps them by folder; a chat's conversation is the
/// app's own. What is there already stays. Not for the UI thread.
fn carry_session(profile: &crate::agents::Profile, from: &Path, to: &Path, session: Option<&str>, tag: Option<&str>) -> Vec<PathBuf> {
    let mut made = Vec::new();
    let mut copy = |from: &Path, to: &Path| {
        if to.exists() {
            return;
        }
        if let Some(dir) = to.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::copy(from, to).is_ok() {
            made.push(to.to_path_buf());
        }
    };
    if profile.acp {
        let (from, to) = (crate::acp::journal_found(from, tag), crate::acp::journal_path(to));
        // A remote task's: on its host.
        if crate::hosts::is_remote(&to) {
            if crate::hostfs::stat(&to).is_err() && crate::hostfs::read(&from).and_then(|bytes| crate::hostfs::write(&to, &bytes)).is_ok() {
                made.push(to);
            }
            return made;
        }
        copy(&from, &to);
        return made;
    }
    let adapter = profile.adapter();
    let (Some(found), Some(base)) = (session.and_then(|id| adapter.session_files(profile, from, Some(id))), adapter.session_base(profile, to)) else { return made };
    // Kept in one place for every folder (Codex): nothing to carry.
    if found.base == base {
        return made;
    }
    for rel in &found.files {
        copy(&found.base.join(rel), &base.join(rel));
    }
    made
}

/// What a question starts from: nothing, or an agent of the project.
fn context_picker(sources: Rc<Vec<QuestionSource>>, chosen: Rc<RefCell<Option<QuestionSource>>>) -> impl IntoElement {
    let name = chosen.borrow().as_ref().map_or("Start empty".to_string(), |source| source.label.clone());
    Button::new("question-context").xsmall().outline().label(format!("{name} ▾")).tooltip("What the question's agent knows when it starts").dropdown_menu(move |menu, _, _| {
        let empty = chosen.clone();
        let mut menu = menu.scrollable(true).max_h(px(320.)).item(PopupMenuItem::new("Start empty").checked(chosen.borrow().is_none()).on_click(move |_, window, _| {
            *empty.borrow_mut() = None;
            window.refresh();
        }));
        for source in sources.iter() {
            let (picked, pick) = (source.clone(), chosen.clone());
            let checked = chosen.borrow().as_ref().is_some_and(|c| c.session == source.session);
            menu = menu.item(PopupMenuItem::new(source.label.clone()).checked(checked).on_click(move |_, window, _| {
                *pick.borrow_mut() = Some(picked.clone());
                window.refresh();
            }));
        }
        menu
    })
}

/// What a question's agent is told in the task it became: where it is
/// now, and to move what it wrote in the folder it was asked in.
fn moved_here(q: &state::QuestionState, from: &Path, from_branch: &str, to: &Path, branch: &str) -> String {
    let mut told = format!(
        "You were answering a question in {} (branch {from_branch}), beside another agent working there. It is now a task of its own: you are in a worktree made for it, {}, on branch {branch}, and may change files here.",
        from.display(),
        to.display(),
    );
    if q.session.is_none() {
        told.push_str(&format!("\n\nThe question was: {}", q.title));
    }
    if q.wrote.is_empty() {
        told.push_str("\n\nIf you wrote anything there for this, move it here and remove it there. Then go on with it.");
    } else {
        let list: Vec<String> = q.wrote.iter().map(|f| format!("- {f}")).collect();
        told.push_str(&format!(
            "\n\nYou wrote these files there, relative to {}:\n{}\n\nMove into this worktree those you wrote for this (as they are now there: the other agent may have changed some), remove them there, and leave alone any that were not yours to begin with. Then go on with it.",
            from.display(),
            list.join("\n")
        ));
    }
    told
}

/// How a question's agent starts again: its session by id. One whose id
/// never came starts a new one, not the latest in the folder.
fn question_launch(q: &state::QuestionState) -> Option<Launch> {
    let profile = state::with_settings(|s| s.profile(&q.profile).cloned())?;
    let start = match &q.session {
        Some(id) => Start::Resume(id.clone()),
        None => Start::New(None),
    };
    Some(Launch { profile, start, unasked: false, aside: true, options: Vec::new(), host: None })
}

fn prompt_says_more(prompt: &str, title: &str) -> bool {
    prompt.trim().chars().count() > title.chars().count() + 10 || title.chars().count() > 50
}

/// A few words for what is asked, by the agent for quick questions; none
/// when there is no such agent or it did not answer. Not for the UI thread.
fn name_question(prompt: &str) -> Option<String> {
    quick_title(format!(
        "Give a title of at most six words for the question below, asked about a code repository, \
         in the question's language. Answer with the title only, no quotes, no period.\n\n{}",
        crate::said::cap(prompt, 4000)
    ))
}

/// A task named by its ticket's key alone: what the ticket asks for, from
/// what its agent said after reading it. Not on the UI thread.
pub(super) fn name_ticket(key: &str, said: &str) -> Option<String> {
    let name = quick_title(format!(
        "An agent was given the ticket {key} and, after reading it, said what follows. Give a title of at most \
         six words for the work the ticket asks for, in the ticket's language, without the key. Answer with the \
         title only, no quotes, no period.\n\n{}",
        crate::said::cap(said, 4000)
    ))?;
    let name = name.trim_start_matches(key).trim_start_matches([':', ' ', '-']).to_string();
    (!name.is_empty()).then_some(name)
}

/// One line the agent for quick questions answers `text` with.
fn quick_title(text: String) -> Option<String> {
    use std::io::Write as _;
    let profile = state::with_settings(|s| s.quick_profile().cloned())?;
    let asked = profile.adapter().one_shot(&profile, &crate::agents::Ask::Quick)?;
    let mut child = asked.command().stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let out = child.wait_with_output().ok().filter(|out| out.status.success())?;
    let name = asked.read_answer(&String::from_utf8_lossy(&out.stdout)).0;
    let name = name.lines().next()?.trim().trim_matches(['"', '\'', '`', '.']).trim();
    (!name.is_empty()).then(|| crate::said::cap(name, 80))
}

/// A question's title: its first line, as long as a row shows.
fn first_line(prompt: &str) -> String {
    let line = prompt.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    crate::said::cap(line, 80)
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::Workspace;
    use crate::state::{self, ProjectState, State, TaskState};
    use gpui_kit::SharedString;

    /// A question asked of a project whose main thread has a session of
    /// before, its id never told: that one is found and kept, the question's
    /// agent starts alone, reports as its own, hides, closes and comes back;
    /// the main thread's agent, started later, resumes its own session.
    ///
    /// `cargo test -- --ignored a_question_beside` after changing questions
    /// or how a task's agents are told apart.
    #[gpui_kit::test]
    #[ignore]
    fn a_question_beside_the_main_thread(cx: &mut gpui_kit::TestAppContext) {
        use crate::agent::{AgentStatus, HookEvent};
        use crate::agents::Profile;
        use crate::task::Who;
        use gpui_kit::{AppContext as _, Bounds, Point, WindowBounds, WindowOptions, px, size};
        use std::os::unix::fs::PermissionsExt as _;
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        cx.executor().allow_parking();
        let dir = std::env::temp_dir().join(format!("ideae-questions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        assert!(std::process::Command::new("sh").arg("-ec").arg("git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t; echo one > f.txt; git add f.txt; git commit -qm base").current_dir(&dir).status().unwrap().success());
        let repo = dir.join("repo").canonicalize().unwrap();
        // Claude Code's stand-in: how it was started, by which agent, then it waits.
        // Asked once (`-p`, a question's title), it says nothing: it runs where
        // the tests do, in the app's own checkout.
        let claude = dir.join("bin").join("claude");
        std::fs::write(&claude, "#!/bin/sh\n[ \"$1\" = -p ] && exit 0\necho \"$@\" > \"$PWD/.args-$IDEAE_AGENT\"\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The main thread's session of before, in Claude Code's folder for the repository.
        let config = dir.join("claude-config");
        let transcripts = config.join("projects").join(crate::agents::claude::project_dir(&repo));
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(transcripts.join("main-before.jsonl"), "{}\n").unwrap();

        // SAFETY: set before anything of the app reads them.
        unsafe {
            std::env::set_var("IDEAE_STATE", dir.join("state.json"));
            std::env::set_var("IDEAE_NO_DAEMON", "1");
        }
        let main = TaskState { path: repo.clone(), main: true, sessions: vec![state::AgentSession { profile: "claude".into(), id: None, started: 1 }], ..Default::default() };
        let saved = State { projects: vec![ProjectState { path: repo.clone(), tasks: vec![main], ..Default::default() }], ..Default::default() };
        std::fs::write(dir.join("state.json"), serde_json::to_string(&saved).unwrap()).unwrap();
        let mut settings = state::Settings { notifications: false, pr_status: false, ..Default::default() };
        let mut profile = Profile { command: claude.display().to_string(), ..Profile::claude_code() };
        profile.env.insert("CLAUDE_CONFIG_DIR".into(), config.display().to_string());
        settings.agents = vec![profile];
        state::set_settings(settings);
        cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_cn::init(cx);
            crate::theme::init(cx);
        });
        let (handle, workspace) = cx.update(|cx| {
            let bounds = Bounds { origin: Point::default(), size: size(px(1300.), px(800.)) };
            let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(Vec::new(), None, None, window, cx))).expect("a test window")
        });
        let step = |cx: &mut gpui_kit::TestAppContext, act: &mut dyn FnMut(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<Workspace>)| {
            cx.update_window(handle.into(), |_, window, cx| workspace.update(cx, |w, cx| act(w, window, cx))).unwrap();
            cx.run_until_parked();
        };
        let wait = |cx: &mut gpui_kit::TestAppContext, what: &str, done: &mut dyn FnMut(&mut gpui_kit::TestAppContext) -> bool| {
            for _ in 0..1500 {
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                cx.executor().advance_clock(std::time::Duration::from_millis(10));
            }
            panic!("waited in vain for {what}");
        };
        fn main_entry(w: &Workspace) -> &crate::workspace::TaskEntry {
            w.projects[0].tasks.iter().find(|t| t.main).unwrap()
        }
        let args = |who: &str| std::fs::read_to_string(repo.join(format!(".args-{who}"))).unwrap_or_default();

        // As it is asked: typed into its dialog and sent from the keyboard.
        step(cx, &mut |w, window, cx| w.ask_question_dialog(0, window, cx));
        cx.update_window(handle.into(), |_, window, cx| {
            use gpui_kit::test::TestWindowExt as _;
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.simulate_input(handle.into(), "Why does f.txt say one?");
        cx.simulate_keystrokes(handle.into(), "enter");
        cx.simulate_input(handle.into(), "And where is it read?");
        cx.simulate_keystrokes(handle.into(), "secondary-enter");
        wait(cx, "the question's agent to start", &mut |_| !args("q-1").is_empty());
        let asked = args("q-1");
        let id = workspace.update(cx, |w, _| {
            let t = main_entry(w);
            assert_eq!(t.asked, 1);
            assert_eq!(t.sessions[0].id.as_deref(), Some("main-before"), "the main thread's session found while it was the latest");
            assert_eq!(t.questions[0].title, "Why does f.txt say one?");
            t.questions[0].session.clone().expect("an id chosen")
        });
        assert!(asked.contains(&format!("--session-id {id}")) && asked.contains("--append-system-prompt"), "{asked}");
        assert!(args("").is_empty(), "the main thread's own agent not started for a question");

        // Its reports are its own: the main thread's agent is not touched.
        let path = repo.clone();
        let hook = |event: &str, agent: Option<&str>| HookEvent { task: path.clone(), agent: agent.map(str::to_string), event: event.into(), message: None, kind: None, file: None, said: None, replayed: None, session: None, guessed: false, worktree: None, call: None };
        step(cx, &mut |w, window, cx| w.on_hook(hook("UserPromptSubmit", Some("q-1")), window, cx));
        workspace.update(cx, |w, cx| {
            let v = main_entry(w).view.as_ref().unwrap().read(cx);
            assert_eq!(v.agent_of(Who::Question(1)).unwrap().status, AgentStatus::Working);
            assert_eq!(v.agent.status, AgentStatus::Unknown);
        });
        // It asks something: the task waits for you, for it.
        step(cx, &mut |w, window, cx| {
            let asks = HookEvent { kind: Some("permission_prompt".into()), message: Some("Allow: cat f.txt?".into()), ..hook("Notification", Some("q-1")) };
            w.on_hook(asks, window, cx);
        });
        workspace.update(cx, |w, cx| assert_eq!(main_entry(w).view.as_ref().unwrap().read(cx).question_asks(), Some(1)));
        // Its row under the main thread.
        cx.update_window(handle.into(), |_, window, cx| {
            use gpui_kit::test::TestWindowExt as _;
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from(format!("question-{}-1", repo.display()))).is_some(), "the question's row");
        })
        .unwrap();

        // × hides its tab, its agent going on; its row brings it back.
        step(cx, &mut |w, window, cx| {
            let view = main_entry(w).view.clone().unwrap();
            view.update(cx, |t, cx| t.hide_question(1, window, cx));
            assert!(view.read(cx).agent_of(Who::Question(1)).is_some(), "still open");
            w.show_question(&repo, 1, window, cx);
        });

        // The main thread's own agent, opened: its session of before.
        step(cx, &mut |w, window, cx| {
            let view = main_entry(w).view.clone().unwrap();
            view.update(cx, |t, cx| t.show_view(0, window, cx));
        });
        wait(cx, "the main thread's agent", &mut |_| !args("").is_empty());
        assert!(args("").contains("--resume main-before"), "{}", args(""));

        // Closed: its agent ends, it is kept as closed; asked again, it goes on.
        step(cx, &mut |w, window, cx| w.close_question(&repo, 1, window, cx));
        workspace.update(cx, |w, cx| {
            let t = main_entry(w);
            assert!(t.questions[0].closed > 0);
            assert!(t.view.as_ref().unwrap().read(cx).agent_of(Who::Question(1)).is_none());
        });
        let _ = std::fs::remove_file(repo.join(".args-q-1"));
        step(cx, &mut |w, window, cx| w.reopen_question(&repo, 1, window, cx));
        wait(cx, "the question asked again", &mut |_| !args("q-1").is_empty());
        assert!(args("q-1").contains(&format!("--resume {id}")), "{}", args("q-1"));

        step(cx, &mut |w, _, cx| {
            let snapshot = w.snapshot(cx);
            let main = snapshot.projects[0].tasks.iter().find(|t| t.main).unwrap();
            assert_eq!((main.asked, main.questions.len()), (1, 1), "kept in the state");
        });

        // Done and out of sight, it sleeps: its agent ends, the question stays; shown, it resumes.
        step(cx, &mut |w, window, cx| {
            w.on_hook(hook("Stop", Some("q-1")), window, cx);
            let view = main_entry(w).view.clone().unwrap();
            view.update(cx, |t, cx| {
                t.hide_question(1, window, cx);
                t.sleep_idle_questions(std::time::Duration::ZERO, cx);
                assert!(t.question_asleep(1) && t.agent_of(Who::Question(1)).is_some(), "asleep, still open");
            });
        });
        let _ = std::fs::remove_file(repo.join(".args-q-1"));
        step(cx, &mut |w, window, cx| w.show_question(&repo, 1, window, cx));
        wait(cx, "the question woken", &mut |_| !args("q-1").is_empty());
        assert!(args("q-1").contains(&format!("--resume {id}")), "{}", args("q-1"));
        // The main thread paused: a question at work is told of, and stays
        // listed; opened, it goes on and the main thread's agent stays stopped.
        step(cx, &mut |w, window, cx| {
            w.on_hook(hook("UserPromptSubmit", Some("q-1")), window, cx);
            w.close_task(&repo, window, cx);
            assert!(main_entry(w).view.is_some(), "asked first");
            {
                use gpui_kit::component::WindowExt as _;
                window.close_dialog(cx);
            }
            w.stop(&repo, window, cx);
            let t = main_entry(w);
            assert!(t.view.is_none() && t.questions[0].asleep);
        });
        cx.update_window(handle.into(), |_, window, cx| {
            use gpui_kit::test::TestWindowExt as _;
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from(format!("question-{}-1", repo.display()))).is_some(), "its row, the task paused");
        })
        .unwrap();
        let _ = std::fs::remove_file(repo.join(".args-q-1"));
        let _ = std::fs::remove_file(repo.join(".args-"));
        step(cx, &mut |w, window, cx| w.show_question(&repo, 1, window, cx));
        wait(cx, "the question to go on", &mut |_| !args("q-1").is_empty());
        assert!(args("q-1").contains(&format!("--resume {id}")), "{}", args("q-1"));
        assert!(args("").is_empty(), "the main thread's agent still paused");
        workspace.update(cx, |w, _| assert!(!main_entry(w).questions[0].asleep));
        let tabs = |cx: &mut gpui_kit::TestAppContext| workspace.update(cx, |w, cx| main_entry(w).view.as_ref().unwrap().read(cx).agent_tabs());
        assert_eq!(tabs(cx), [(Who::Main, true), (Who::Question(1), false)], "the task's agent keeps the first place, not started");
        workspace.update(cx, |w, cx| assert!(!main_entry(w).view.as_ref().unwrap().read(cx).runs_own_agent(), "a question opened does not make it run"));

        // Its answer, with another tab in front, waits to be read: in Needs
        // you and under the project folded, and read when its tab is opened.
        step(cx, &mut |w, window, cx| {
            let view = main_entry(w).view.clone().unwrap();
            view.update(cx, |t, cx| t.show_view(1, window, cx));
            w.on_hook(hook("UserPromptSubmit", Some("q-1")), window, cx);
            w.on_hook(hook("Stop", Some("q-1")), window, cx);
            assert!(main_entry(w).questions[0].unread, "not in front");
            w.projects[0].collapsed = true;
            cx.notify();
        });
        cx.update_window(handle.into(), |_, window, cx| {
            use gpui_kit::test::TestWindowExt as _;
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from(format!("waiting-question-{}-1", repo.display()))).is_some(), "in Needs you");
            assert!(window.try_find(SharedString::from(format!("folded-question-{}-1", repo.display()))).is_some(), "under the folded project");
        })
        .unwrap();
        // The sidebar narrowed to its rail lists them too.
        step(cx, &mut |w, _, cx| {
            w.sidebar_rail = true;
            cx.notify();
        });
        cx.update_window(handle.into(), |_, window, cx| {
            use gpui_kit::test::TestWindowExt as _;
            window.render_frame(cx);
            assert!(window.try_find(SharedString::from(format!("rail-question-project-{}-1", repo.display()))).is_some(), "in the rail");
            assert!(window.try_find(SharedString::from(format!("rail-question-waiting-{}-1", repo.display()))).is_some(), "in the rail's Needs you");
        })
        .unwrap();
        step(cx, &mut |w, _, cx| {
            w.sidebar_rail = false;
            cx.notify();
        });
        step(cx, &mut |w, window, cx| {
            w.projects[0].collapsed = false;
            w.show_question(&repo, 1, window, cx);
        });
        workspace.update(cx, |w, _| assert!(!main_entry(w).questions[0].unread, "read"));

        // What it wrote is told as its own, until its next prompt.
        step(cx, &mut |w, window, cx| {
            let wrote = HookEvent { file: Some(repo.join("note.md").display().to_string()), message: Some("Writing".into()), ..hook("PreToolUse", Some("q-1")) };
            w.on_hook(wrote, window, cx);
            assert!(main_entry(w).view.as_ref().unwrap().read(cx).question_wrote("note.md"));
            w.on_hook(hook("UserPromptSubmit", Some("q-1")), window, cx);
            assert!(!main_entry(w).view.as_ref().unwrap().read(cx).question_wrote("note.md"));
            w.on_hook(hook("Stop", Some("q-1")), window, cx);
        });

        // Asleep, it keeps its place among the tabs, and goes on in it.
        step(cx, &mut |w, window, cx| {
            let view = main_entry(w).view.clone().unwrap();
            view.update(cx, |t, cx| {
                t.show_view(1, window, cx);
                t.sleep_idle_questions(std::time::Duration::ZERO, cx);
            });
        });
        assert_eq!(tabs(cx)[1], (Who::Question(1), true));
        let _ = std::fs::remove_file(repo.join(".args-q-1"));
        step(cx, &mut |w, window, cx| w.show_question(&repo, 1, window, cx));
        wait(cx, "the question woken in its tab", &mut |_| !args("q-1").is_empty());
        assert_eq!(tabs(cx)[1], (Who::Question(1), false));

        // Started late, the task's agent takes the first tab; a tab dragged
        // goes where it is dropped, not before it.
        step(cx, &mut |w, window, cx| {
            let view = main_entry(w).view.clone().unwrap();
            view.update(cx, |t, cx| {
                t.show_view(0, window, cx);
                let (first, question) = (t.tab_id(0), t.tab_id(1));
                t.reorder_tab(question, first, window, cx);
            });
        });
        wait(cx, "the task's own agent", &mut |_| !args("").is_empty());
        assert_eq!(tabs(cx), [(Who::Main, false), (Who::Question(1), false)]);

        // A pause and back: the question's tab is where it was, its agent not started.
        step(cx, &mut |w, window, cx| {
            w.on_hook(hook("Stop", Some("q-1")), window, cx);
            w.stop(&repo, window, cx);
            w.switch(&repo, window, cx);
        });
        assert_eq!(tabs(cx), [(Who::Main, false), (Who::Question(1), true)]);

        // Left for days, it is closed by itself.
        step(cx, &mut |w, window, cx| {
            let entry = w.entry_mut(&repo).unwrap();
            let q = entry.questions.iter_mut().find(|q| q.id == 1).unwrap();
            (q.created, q.active) = (1, 1);
            w.tend_questions(window, cx);
            assert!(main_entry(w).questions.iter().find(|q| q.id == 1).unwrap().closed > 0);
        });

        // Another question writes a note, then goes on as a task of its own.
        step(cx, &mut |w, window, cx| w.ask(0, "Sketch a plan for reading f.txt".into(), String::new(), None, None, window, cx));
        wait(cx, "the second question's agent", &mut |_| !args("q-2").is_empty());
        let second = workspace.update(cx, |w, _| main_entry(w).questions.iter().find(|q| q.id == 2).and_then(|q| q.session.clone()).expect("its id"));
        std::fs::write(transcripts.join(format!("{second}.jsonl")), "{\"second\":1}\n").unwrap();
        step(cx, &mut |w, window, cx| {
            let wrote = HookEvent { file: Some(repo.join("plan.md").display().to_string()), message: Some("Writing plan.md".into()), ..hook("PreToolUse", Some("q-2")) };
            w.on_hook(wrote, window, cx);
        });
        workspace.update(cx, |w, _| assert_eq!(main_entry(w).questions.iter().find(|q| q.id == 2).unwrap().wrote, ["plan.md"]));
        step(cx, &mut |w, window, cx| w.question_to_task(&repo, 2, window, cx));
        let task = workspace.update(cx, |w, _| {
            assert!(main_entry(w).questions.iter().all(|q| q.id != 2), "no longer a question of the main thread");
            w.projects[0].tasks.iter().find(|t| !t.main).map(|t| t.path.clone()).expect("a task made of it")
        });
        wait(cx, "the task's agent", &mut |_| task.join(".args-").is_file());
        let resumed = std::fs::read_to_string(task.join(".args-")).unwrap();
        assert!(resumed.contains(&format!("--resume {second}")) && !resumed.contains("--append-system-prompt"), "its session, free to change files: {resumed}");
        let carried = config.join("projects").join(crate::agents::claude::project_dir(&task)).join(format!("{second}.jsonl"));
        assert!(carried.is_file(), "its conversation where Claude Code looks for the worktree");

        step(cx, &mut |w, _, cx| {
            for view in w.views() {
                view.update(cx, |t, cx| t.shut_down_at_quit(cx));
            }
        });
        let _ = std::process::Command::new("git").args(["worktree", "remove", "--force"]).arg(&task).current_dir(&repo).status();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
