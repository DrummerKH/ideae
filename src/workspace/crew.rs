//! Agents a task's agent starts through ideae's MCP server, in its own
//! project only: a question beside the main thread (it can only read, so
//! it starts at once), or a task of its own, which the user allows first.
//! The agent gets an id at once and waits for what that one says with
//! `wait_for`, as long as it likes, in calls of a few minutes each; a
//! task's work it reads with `agent_changes`.
//!
//! None of them starts agents of its own: not a question's agent, not the
//! agent of a task an agent started. Their ids are kept in the state, the
//! latest hundred: an agent resumed after a restart can still wait for them.

use super::*;
use std::collections::HashMap;
use crate::agent::AgentStatus;
use crate::mcp::{Answer, Request};
use crate::task::{AgentState, Who};

/// Agents one task's agent has at work at once, at most.
const MOST: usize = 4;
/// How long a `wait_for` waits, unless told; at most.
const WAIT: u64 = 300;
const WAIT_MOST: u64 = 600;

#[derive(Clone, PartialEq)]
enum Child {
    /// Being asked: its number comes once its agent is set up.
    Asking,
    Question { main: PathBuf, n: u32 },
    /// Waiting for the user to allow it, in Needs you.
    Proposed { title: String, prompt: String },
    Task(PathBuf),
    Declined,
    /// It could not be started: why.
    Failed(String),
}

struct Started {
    parent: PathBuf,
    child: Child,
    /// When it was last told something: what it says after that is its
    /// answer. None when known from the state, after a restart: what it said
    /// last is its answer then.
    told: Option<std::time::Instant>,
}

#[derive(Default)]
pub struct Crew {
    next: u32,
    started: HashMap<u32, Started>,
}

/// What one started is at, as `wait_for` tells it.
enum Now {
    Busy(String),
    Over(String),
}

impl Workspace {
    /// `ask_agent`, its id given (by the store; none: the window's own).
    #[allow(clippy::too_many_arguments)]
    fn ask_for_crew(&mut self, parent: PathBuf, question: String, with_context: bool, given: Option<u32>, answer: async_channel::Sender<Answer>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pi, _)) = self.find(&parent) else {
            let _ = answer.try_send(Answer::error("This task is not open in ideae now."));
            return;
        };
        let id = self.crew.add(parent.clone(), Child::Asking, given);
        let by = parent.clone();
        let then: questions::AskedThen = Box::new(move |this, main, n, cx| {
            if let Some(q) = n.and_then(|n| this.entry_mut(main).and_then(|e| e.questions.iter_mut().find(|q| q.id == n))) {
                q.asked_by = Some(by);
            }
            if let Some(started) = this.crew.started.get_mut(&id) {
                started.child = match n {
                    Some(n) => Child::Question { main: main.to_path_buf(), n },
                    None => Child::Failed("It could not be asked: no agent to ask it (ideae: Settings → Agents).".into()),
                };
                started.told = Some(std::time::Instant::now());
            }
            cx.notify();
        });
        let from = with_context.then_some(parent.as_path());
        self.ask_for_agent(pi, question, from, then, window, cx);
        self.save_later(cx);
        let _ = answer.try_send(Answer::ok(format!("Asked, as id {id}. wait_for with id {id} gives its answer.")));
    }

    /// `ask_agent`, `start_task`, `wait_for`, `tell_agent`, for the task whose
    /// worktree the request names (its token was checked).
    pub(super) fn on_crew(&mut self, request: Request, answer: async_channel::Sender<Answer>, window: &mut Window, cx: &mut Context<Self>) {
        let reply = |a: Answer| {
            let _ = answer.try_send(a);
        };
        let parent = request.root.clone();
        let text = |key: &str| request.args.get(key).and_then(serde_json::Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let id = request.args.get("id").and_then(serde_json::Value::as_u64).map(|id| id as u32);
        // Turned off since the agent started: its server still lists the tools.
        if !state::with_settings(|s| s.agents_start_agents) {
            return reply(Answer::error("The user has not allowed agents to start other agents (ideae: Settings → Agents)."));
        }
        // A question's agent shares its folder, and so its token, with the main thread's:
        // what that one started is not its to wait for, tell or read.
        if request.agent.is_some() {
            return reply(Answer::error("A question's agent cannot start other agents, nor use those another started."));
        }
        match request.tool.as_str() {
            "ask_agent" => {
                let Some(question) = text("question") else { return reply(Answer::error("No question given.")) };
                if let Err(why) = self.may_start(&request, cx) {
                    return reply(Answer::error(why));
                }
                if self.find(&parent).is_none() {
                    return reply(Answer::error("This task is not open in ideae now."));
                }
                let with_context = request.args.get("with_my_context").and_then(serde_json::Value::as_bool).unwrap_or(false);
                // Its id the machine's store's (another window's agents ask too), given off the UI thread.
                cx.spawn_in(window, async move |this, cx| {
                    let given = cx.background_spawn(async { crate::machine::reserve(None) }).await;
                    let _ = this.update_in(cx, |this, window, cx| this.ask_for_crew(parent, question, with_context, given, answer, window, cx));
                })
                .detach();
            }
            "start_task" => {
                let (Some(title), Some(prompt)) = (text("title"), text("prompt")) else { return reply(Answer::error("A title and a prompt are needed.")) };
                if let Err(why) = self.may_start(&request, cx) {
                    return reply(Answer::error(why));
                }
                cx.spawn_in(window, async move |this, cx| {
                    let given = cx.background_spawn(async { crate::machine::reserve(None) }).await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        let id = this.crew.add(parent.clone(), Child::Proposed { title: title.clone(), prompt }, given);
                        this.propose_task(&parent, &title, window, cx);
                        this.save_later(cx);
                        let _ = answer.try_send(Answer::ok(format!(
                            "Proposed to the user, as id {id}. wait_for with id {id} tells whether it was allowed, and then what its agent says when its turn ends."
                        )));
                    });
                })
                .detach();
            }
            "wait_for" => {
                let Some(id) = id.filter(|id| self.crew.started.get(id).is_some_and(|s| s.parent == parent)) else {
                    return reply(Answer::error("No agent of that id was started by this task's agent."));
                };
                let seconds = request.args.get("timeout_seconds").and_then(serde_json::Value::as_u64).unwrap_or(WAIT).clamp(1, WAIT_MOST);
                let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
                cx.spawn_in(window, async move |this, cx| {
                    loop {
                        let now = this.update(cx, |w, cx| w.child_now(id, cx)).ok().flatten();
                        let said = match now {
                            None => Answer::error("It is gone."),
                            Some(Now::Over(text)) => Answer::ok(text),
                            Some(Now::Busy(doing)) if std::time::Instant::now() >= until => {
                                Answer::ok(format!("Still at it: {doing}. Call wait_for with id {id} again to wait longer."))
                            }
                            Some(Now::Busy(_)) if answer.is_closed() => return,
                            Some(Now::Busy(_)) => {
                                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                                continue;
                            }
                        };
                        let _ = answer.try_send(said);
                        return;
                    }
                })
                .detach();
            }
            "agent_changes" => {
                let Some(started) = id.and_then(|id| self.crew.started.get(&id)).filter(|s| s.parent == parent) else {
                    return reply(Answer::error("No agent of that id was started by this task's agent."));
                };
                let Child::Task(path) = started.child.clone() else {
                    return reply(Answer::error("Only a task changes files: a question only reads."));
                };
                let Some(base) = self.entry_mut(&path).map(|e| e.view.as_ref().and_then(|v| v.read(cx).base_ref().map(str::to_string))) else {
                    return reply(Answer::error("The task is gone: archived or deleted."));
                };
                cx.background_spawn(crate::procs::on_thread(move || {
                    let _ = answer.try_send(match changes_of(&path, base.as_deref().unwrap_or("origin/HEAD")) {
                        Ok(text) => Answer::ok(text),
                        Err(e) => Answer::error(e),
                    });
                }))
                .detach();
            }
            "tell_agent" => {
                let Some(message) = text("message") else { return reply(Answer::error("No message given.")) };
                let Some(started) = id.and_then(|id| self.crew.started.get(&id)).filter(|s| s.parent == parent) else {
                    return reply(Answer::error("No agent of that id was started by this task's agent."));
                };
                let told = match started.child.clone() {
                    Child::Question { main, n } => self.entry_mut(&main).and_then(|e| e.view.clone()).map(|view| {
                        view.update(cx, |t, cx| {
                            if t.question_asleep(n) {
                                t.show_question(n, window, cx);
                            }
                            t.queue_prompt_to(Who::Question(n), message, true, window, cx);
                        });
                        true
                    }),
                    // Asleep or not started, it is started and given the message once up;
                    // stopped, it is not there to take it.
                    Child::Task(path) => self.entry_mut(&path).and_then(|e| e.view.clone()).map(|view| {
                        view.update(cx, |t, cx| {
                            if t.agent_slept() {
                                t.wake_agent(window, cx);
                            }
                            match t.agent.status {
                                AgentStatus::Unknown => {
                                    t.queue_prompt(message, true, window, cx);
                                    true
                                }
                                _ => !matches!(t.message_agent(message, window, cx), crate::task::Told::Not),
                            }
                        })
                    }),
                    Child::Asking | Child::Proposed { .. } => return reply(Answer::error("It has not started yet: wait_for first.")),
                    Child::Declined => return reply(Answer::error("The user declined it.")),
                    Child::Failed(why) => return reply(Answer::error(why)),
                };
                match told {
                    None => return reply(Answer::error("It is not open in ideae now.")),
                    Some(false) => return reply(Answer::error("Its agent has stopped: the user can start it again in its task.")),
                    Some(true) => {}
                }
                if let Some(started) = id.and_then(|id| self.crew.started.get_mut(&id)) {
                    started.told = Some(std::time::Instant::now());
                }
                reply(Answer::ok("Sent. wait_for gives its answer."));
            }
            other => reply(Answer::error(format!("ideae has no tool {other}"))),
        }
    }

    /// Only a task's own agent, not one an agent started, and not too many
    /// at work at once.
    fn may_start(&self, request: &Request, cx: &App) -> Result<(), String> {
        if request.agent.is_some() {
            return Err("A question's agent cannot start other agents.".into());
        }
        let (pi, ti) = self.find(&request.root).ok_or("This task is not open in ideae now.")?;
        if self.projects[pi].tasks[ti].spawned_by.is_some() {
            return Err("An agent started by another agent cannot start more.".into());
        }
        let ids: Vec<u32> = self.crew.started.iter().filter(|(_, s)| s.parent == request.root).map(|(id, _)| *id).collect();
        let busy = ids.into_iter().filter(|id| matches!(self.child_now(*id, cx), Some(Now::Busy(_)))).count();
        if busy >= MOST {
            return Err(format!("{MOST} agents you started are at work already: wait for them first."));
        }
        Ok(())
    }

    /// Told: the proposal waits in Needs you, where it is allowed or
    /// declined when you get to it; nothing comes over what you do.
    fn propose_task(&mut self, parent: &Path, title: &str, window: &mut Window, cx: &mut Context<Self>) {
        let by = self.find(parent).map(|(pi, ti)| self.projects[pi].tasks[ti].label()).unwrap_or_default();
        let told = format!("The agent of {by} asks to start a task: {title}. It waits in Needs you.");
        if window.is_window_active() {
            window.push_notification(Notification::info(told).title("A task proposed"), cx);
        } else if state::settings().notifications {
            let (tx, path) = (self.open_task_tx.clone(), parent.to_path_buf());
            crate::desktop::notify_with("A task proposed", &told, move || {
                let _ = tx.send_blocking((path, None));
            });
        }
        cx.notify();
    }

    /// Tasks proposed by agents, waiting for an answer: id, whose agent,
    /// title, prompt.
    pub(crate) fn proposals(&self) -> Vec<(u32, PathBuf, String, String)> {
        let mut all: Vec<_> = self
            .crew
            .started
            .iter()
            .filter_map(|(id, s)| match &s.child {
                Child::Proposed { title, prompt } => Some((*id, s.parent.clone(), title.clone(), prompt.clone())),
                _ => None,
            })
            .collect();
        all.sort_by_key(|p| p.0);
        all
    }

    /// Allowed, the task is made out of view, its agent sent the prompt.
    pub(crate) fn answer_proposal(&mut self, id: u32, allowed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(started) = self.crew.started.get(&id) else { return };
        let Child::Proposed { title, prompt } = started.child.clone() else { return };
        let parent = started.parent.clone();
        let made = allowed.then(|| self.find(&parent).map(|(pi, _)| pi)).flatten().and_then(|pi| {
            let before = self.projects[pi].tasks.len();
            // Sent: its agent's task, nobody to press Enter.
            self.create_worktree_task(pi, title, prompt, vec![String::new()], Vec::new(), true, window, cx);
            let entry = self.projects[pi].tasks.get_mut(before)?;
            entry.spawned_by = Some(parent.clone());
            Some(entry.path.clone())
        });
        if let Some(started) = self.crew.started.get_mut(&id) {
            started.child = made.map_or(Child::Declined, Child::Task);
            started.told = Some(std::time::Instant::now());
        }
        self.save_later(cx);
        cx.notify();
    }

    /// Pause or Archive of a task whose agent started tasks: asked first
    /// whether they go too (they would run on with nobody asking for them).
    pub(crate) fn end_with_children(&mut self, path: &Path, archive: bool, window: &mut Window, cx: &mut Context<Self>) {
        let end = move |w: &mut Self, p: &Path, window: &mut Window, cx: &mut Context<Self>| if archive { w.archive_task(p, window, cx) } else { w.close_task(p, window, cx) };
        let children: Vec<PathBuf> = self
            .projects
            .iter()
            .flat_map(|p| &p.tasks)
            .filter(|t| t.spawned_by.as_deref() == Some(path) && t.busy.is_none() && (archive || t.view.is_some()))
            .map(|t| t.path.clone())
            .collect();
        if children.is_empty() {
            return end(self, path, window, cx);
        }
        let (verb, n) = (if archive { "Archive" } else { "Pause" }, children.len());
        let this = cx.weak_entity();
        let path = path.to_path_buf();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, path, children) = (this.clone(), path.clone(), children.clone());
            let (alone_this, alone_path) = (this.clone(), path.clone());
            dialog
                .confirm()
                .title(format!("{verb} the {} its agent started too?", if n == 1 { "task".to_string() } else { format!("{n} tasks") }))
                .description("They were started for this task's work. Left alone, they go on as tasks of their own.")
                .ok_text(format!("{verb} them too"))
                .cancel_text("Only this one")
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |w, cx| {
                        for child in &children {
                            end(w, child, window, cx);
                        }
                        end(w, &path, window, cx);
                    });
                    true
                })
                .on_cancel(move |_, window, cx| {
                    let _ = alone_this.update(cx, |w, cx| end(w, &alone_path, window, cx));
                    true
                })
        });
    }

    fn child_now(&self, id: u32, cx: &App) -> Option<Now> {
        let started = self.crew.started.get(&id)?;
        Some(match started.child.clone() {
            Child::Asking => Now::Busy("being set up".into()),
            Child::Proposed { .. } => Now::Busy("waiting for the user to allow it".into()),
            Child::Declined => Now::Over("The user declined it.".into()),
            Child::Failed(why) => Now::Over(why),
            Child::Question { main, n } => {
                let (pi, ti) = self.find(&main)?;
                let entry = &self.projects[pi].tasks[ti];
                let Some(kept) = entry.questions.iter().find(|q| q.id == n).filter(|q| q.closed == 0) else {
                    return Some(Now::Over("The user closed this question.".into()));
                };
                // After a restart its answer is the one kept, its agent maybe not running.
                let answered = (started.told.is_none() && !kept.answer.is_empty()).then(|| kept.answer.clone());
                let agent = entry.view.as_ref().and_then(|v| v.read(cx).questions.iter().find(|q| q.id == n).map(|q| (state_of(&q.agent, started.told), q.agent.status.clone())));
                match (agent, answered) {
                    // At work on something told before: not the answer kept.
                    (Some((now, AgentStatus::Working | AgentStatus::NeedsInput(_))), _) => now,
                    (Some((Now::Busy(_), _)), Some(answer)) | (None, Some(answer)) => Now::Over(answer),
                    (Some((now, _)), _) => now,
                    (None, None) => Now::Busy("not running now".into()),
                }
            }
            Child::Task(path) => {
                let Some((pi, ti)) = self.find(&path) else { return Some(Now::Over("The task is gone: archived or deleted.".into())) };
                let entry = &self.projects[pi].tasks[ti];
                match &entry.view {
                    Some(view) if entry.busy.is_none() => state_of(&view.read(cx).agent, started.told),
                    None if entry.busy.is_none() => Now::Over("Its task is paused; agent_changes shows its work.".into()),
                    _ => Now::Busy("its worktree being made".into()),
                }
            }
        })
    }
}

impl Crew {
    /// As kept in the state: one being asked is not, it has no number yet.
    pub fn records(&self) -> Vec<state::CrewRecord> {
        let mut all: Vec<state::CrewRecord> = self
            .started
            .iter()
            .filter_map(|(&id, s)| {
                let record = state::CrewRecord { id, parent: s.parent.clone(), ..Default::default() };
                Some(match &s.child {
                    Child::Asking | Child::Failed(_) => return None,
                    Child::Question { main, n } => state::CrewRecord { kind: "question".into(), path: Some(main.clone()), n: *n, ..record },
                    Child::Proposed { title, prompt } => state::CrewRecord { kind: "proposed".into(), title: title.clone(), prompt: prompt.clone(), ..record },
                    Child::Task(path) => state::CrewRecord { kind: "task".into(), path: Some(path.clone()), ..record },
                    Child::Declined => state::CrewRecord { kind: "declined".into(), ..record },
                })
            })
            .collect();
        all.sort_by_key(|r| r.id);
        let old = all.len().saturating_sub(100);
        all.drain(..old);
        all
    }

    pub fn from_records(records: &[state::CrewRecord]) -> Self {
        let mut crew = Crew::default();
        for r in records {
            let child = match (r.kind.as_str(), &r.path) {
                ("question", Some(main)) => Child::Question { main: main.clone(), n: r.n },
                ("task", Some(path)) => Child::Task(path.clone()),
                ("proposed", _) => Child::Proposed { title: r.title.clone(), prompt: r.prompt.clone() },
                ("declined", _) => Child::Declined,
                _ => continue,
            };
            crew.started.insert(r.id, Started { parent: r.parent.clone(), child, told: None });
            crew.next = crew.next.max(r.id);
        }
        crew
    }

    /// `given`: its id, by the machine's store; none, the next of this window's.
    fn add(&mut self, parent: PathBuf, child: Child, given: Option<u32>) -> u32 {
        // Ids go on from the highest kept, never given twice.
        let id = given.filter(|id| !self.started.contains_key(id)).unwrap_or(self.next + 1);
        self.next = self.next.max(id);
        self.started.insert(id, Started { parent, child, told: Some(std::time::Instant::now()) });
        id
    }

    /// The records another window keeps (by way of the store): theirs in;
    /// this window's own kept that are under way (being asked), not yet
    /// brought to the store (not in `before`, what it had of the store) or
    /// changed here since (a proposal allowed, not yet brought).
    pub fn take_in(&mut self, records: &[state::CrewRecord], before: &[state::CrewRecord]) {
        let theirs = Crew::from_records(records);
        let mine: std::collections::HashMap<u32, state::CrewRecord> = self.records().into_iter().map(|r| (r.id, r)).collect();
        let had = |id: &u32| before.iter().find(|r| r.id == *id);
        let changed_here = |id: &u32| mine.get(id).is_some_and(|m| had(id) != Some(m));
        self.started.retain(|id, s| theirs.started.contains_key(id) || matches!(s.child, Child::Asking | Child::Failed(_)) || had(id).is_none() || changed_here(id));
        for (id, started) in theirs.started {
            if changed_here(&id) {
                continue;
            }
            let same = self.started.get(&id).is_some_and(|m| m.parent == started.parent && m.child == started.child);
            if !same {
                self.started.insert(id, started);
            }
        }
        self.next = self.next.max(theirs.next);
    }
}

/// A task's commits since it left `base`, its diff against that point
/// (what is not committed too) and its new files; the diff cut to a size
/// an agent reads. Not on the UI thread.
fn changes_of(root: &Path, base: &str) -> Result<String, String> {
    const MOST: usize = 60_000;
    let git = |args: &[&str]| -> Result<String, String> {
        let out = crate::procs::command("git").current_dir(root).args(args).output().map_err(|e| e.to_string())?;
        if out.status.success() { Ok(String::from_utf8_lossy(&out.stdout).into_owned()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
    };
    let fork = git(&["merge-base", "HEAD", base]).map(|s| s.trim().to_string()).unwrap_or_else(|_| "HEAD".into());
    let log = git(&["log", "--format=%h %s", &format!("{fork}..HEAD")])?;
    let stat = git(&["diff", "--stat", &fork])?;
    let diff = git(&["diff", &fork])?;
    let new = git(&["ls-files", "--others", "--exclude-standard"])?;
    let mut text = format!("Branch work since {base} ({fork:.10}), in {}:\n\nCommits:\n{}\n", root.display(), if log.trim().is_empty() { "none\n" } else { &log });
    text.push_str(&format!("Changed files:\n{}\n", if stat.trim().is_empty() { "none\n" } else { &stat }));
    if !new.trim().is_empty() {
        text.push_str(&format!("New files not added:\n{new}\n"));
    }
    if diff.chars().count() > MOST {
        text.push_str(&format!("The diff, its start (read the files for the rest):\n{}\n", diff.chars().take(MOST).collect::<String>()));
    } else if !diff.is_empty() {
        text.push_str(&format!("The diff:\n{diff}"));
    }
    Ok(text)
}

/// Its turn over since it was last told something, or a question for the
/// user: what it said. Else what it is doing. Known from the state (`told`
/// none), a turn over is the last one.
fn state_of(agent: &AgentState, told: Option<std::time::Instant>) -> Now {
    let said = |s: Option<&crate::said::Said>| s.map(|s| s.whole.to_string());
    match &agent.status {
        AgentStatus::NeedsInput(message) => Now::Over(format!("It asks the user: {}", said(agent.asks.as_ref()).unwrap_or_else(|| message.clone()))),
        AgentStatus::Done if told.is_none_or(|told| agent.heard_at.is_some_and(|at| at > told)) => {
            Now::Over(said(agent.said.as_ref()).unwrap_or_else(|| "It ended its turn without a word.".into()))
        }
        AgentStatus::Exited => Now::Over("Its agent has stopped.".into()),
        _ => Now::Busy(agent.doing.clone().unwrap_or_else(|| "at work".into())),
    }
}

#[cfg(test)]
mod take_in_tests {
    use super::{Child, Crew};
    use crate::state;

    #[test]
    fn another_window_s_crew_does_not_take_this_one_s_back() {
        let rec = |id: u32, kind: &str| state::CrewRecord { id, parent: "/w/a".into(), kind: kind.into(), path: (kind == "task").then(|| "/w/t".into()), title: "T".into(), ..Default::default() };
        let before = vec![rec(1, "proposed"), rec(2, "proposed")];
        let mut crew = Crew::from_records(&before);
        // Allowed here, not yet brought: 1 is a task now. 3 is new here.
        crew.started.get_mut(&1).unwrap().child = Child::Task("/w/t".into());
        crew.add("/w/a".into(), Child::Proposed { title: "N".into(), prompt: String::new() }, Some(3));
        // Another window's: 2 declined there, 4 asked there; 1 as it was.
        let theirs = vec![rec(1, "proposed"), rec(2, "declined"), rec(4, "question")];
        crew.take_in(&theirs, &before);
        let now: std::collections::HashMap<u32, String> = crew.records().into_iter().map(|r| (r.id, r.kind)).collect();
        assert_eq!(now.get(&1).map(String::as_str), Some("task"), "its own change kept");
        assert_eq!(now.get(&2).map(String::as_str), Some("declined"), "theirs taken in");
        assert_eq!(now.get(&3).map(String::as_str), Some("proposed"), "its own new one kept");
        assert!(!now.contains_key(&4) || now.get(&4).map(String::as_str) == Some("question"));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Answer, Request, Workspace};
    use crate::agent::HookEvent;
    use crate::state;
    use std::path::Path;
    use crate::state::{ProjectState, State, TaskState};

    /// A task's agent asks another agent of its project and gets its answer;
    /// proposes a task, which the user allows (made out of view, its agent
    /// told) or declines; a question's agent and an agent so started start
    /// none.
    ///
    /// `cargo test -- --ignored agents_start_agents` after changing crew.rs.
    #[gpui_kit::test]
    #[ignore]
    fn agents_start_agents(cx: &mut gpui_kit::TestAppContext) {
        use crate::agents::Profile;
        use gpui_kit::{AppContext as _, Bounds, Point, WindowBounds, WindowOptions, px, size};
        use std::os::unix::fs::PermissionsExt as _;
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        cx.executor().allow_parking();
        let dir = std::env::temp_dir().join(format!("ideae-crew-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        assert!(std::process::Command::new("sh").arg("-ec").arg("git init -q -b main repo; cd repo; git config user.email t@t; git config user.name t; echo one > f.txt; git add f.txt; git commit -qm base").current_dir(&dir).status().unwrap().success());
        let repo = dir.join("repo").canonicalize().unwrap();
        // Claude Code's stand-in: what it was told goes to a file, then it waits.
        let claude = dir.join("bin").join("claude");
        std::fs::write(&claude, "#!/bin/sh\n[ \"$1\" = -p ] && exit 0\nexec cat > \"$PWD/.told-$IDEAE_AGENT\"\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: set before anything of the app reads them.
        unsafe {
            std::env::set_var("IDEAE_STATE", dir.join("state.json"));
            std::env::set_var("IDEAE_NO_DAEMON", "1");
        }
        let main = TaskState { path: repo.clone(), main: true, ..Default::default() };
        let saved = State { projects: vec![ProjectState { path: repo.clone(), tasks: vec![main], ..Default::default() }], ..Default::default() };
        std::fs::write(dir.join("state.json"), serde_json::to_string(&saved).unwrap()).unwrap();
        let mut settings = state::Settings { notifications: false, pr_status: false, agents_start_agents: true, ..Default::default() };
        settings.agents = vec![Profile { command: claude.display().to_string(), ..Profile::claude_code() }];
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
                cx.executor().advance_clock(std::time::Duration::from_millis(100));
            }
            panic!("waited in vain for {what}");
        };
        // A tool called as the MCP server would, its answer waited for.
        let call = |cx: &mut gpui_kit::TestAppContext, root: &Path, agent: Option<&str>, tool: &str, args: serde_json::Value| -> Answer {
            let (tx, rx) = async_channel::bounded(1);
            let request = Request { root: root.to_path_buf(), token: String::new(), tool: tool.into(), args, agent: agent.map(str::to_string) };
            step(cx, &mut |w, window, cx| w.on_mcp(request.clone(), tx.clone(), window, cx));
            let mut got = None;
            wait(cx, tool, &mut |_| {
                got = rx.try_recv().ok();
                got.is_some()
            });
            got.unwrap()
        };
        // What Claude Code says when it is up, which its prompt waits for.
        let up = |cx: &mut gpui_kit::TestAppContext, task: &Path, agent: Option<&str>| {
            let event = HookEvent { task: task.to_path_buf(), agent: agent.map(str::to_string), event: "SessionStart".into(), message: None, kind: None, file: None, said: None, replayed: None, session: None, guessed: false, worktree: None, call: None };
            step(cx, &mut |w, window, cx| w.on_hook(event.clone(), window, cx));
        };
        let id_in = |a: &Answer| a.text.split("id ").nth(1).and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse::<u64>().ok()).expect("an id");

        step(cx, &mut |w, window, cx| w.create_task(0, "Fix login".into(), String::new(), vec![String::new()], Vec::new(), window, cx));
        let mut parent = None;
        wait(cx, "the task", &mut |cx| {
            parent = workspace.update(cx, |w, _| w.projects[0].tasks.iter().find(|t| !t.main && t.busy.is_none() && t.view.is_some()).map(|t| t.path.clone()));
            parent.is_some()
        });
        let parent = parent.unwrap();

        // A question: started at once, out of view; its answer when its turn ends.
        let asked = call(cx, &parent, None, "ask_agent", serde_json::json!({ "question": "Where is the login form?" }));
        assert!(!asked.error, "{}", asked.text);
        let q = id_in(&asked);
        wait(cx, "the question's agent", &mut |_| repo.join(".told-q-1").exists());
        up(cx, &repo, Some("q-1"));
        wait(cx, "the question's agent told", &mut |_| std::fs::read_to_string(repo.join(".told-q-1")).is_ok_and(|t| t.contains("Where is the login form?")));
        assert_eq!(workspace.update(cx, |w, _| w.active.clone()), Some(parent.clone()), "what you look at stays");
        let early = call(cx, &parent, None, "wait_for", serde_json::json!({ "id": q, "timeout_seconds": 1 }));
        assert!(early.text.starts_with("Still at it"), "{}", early.text);
        let said = HookEvent { task: repo.clone(), agent: Some("q-1".into()), event: "Stop".into(), message: None, kind: None, file: None, said: Some("In src/login.rs.".into()), replayed: None, session: None, guessed: false, worktree: None, call: None };
        step(cx, &mut |w, window, cx| w.on_hook(said.clone(), window, cx));
        let answer = call(cx, &parent, None, "wait_for", serde_json::json!({ "id": q }));
        assert_eq!(answer.text, "In src/login.rs.");
        workspace.update(cx, |w, _| {
            let asked = w.projects[0].tasks.iter().find(|t| t.main).and_then(|t| t.questions.iter().find(|q| q.id == 1)).unwrap();
            assert!(asked.asked_by.as_deref() == Some(parent.as_path()) && !asked.unread, "the agent's to read, not yours");
        });
        // As after a restart: the ids from the state, its answer the one kept.
        workspace.update(cx, |w, _| w.crew = super::Crew::from_records(&w.crew.records()));
        assert_eq!(call(cx, &parent, None, "wait_for", serde_json::json!({ "id": q, "timeout_seconds": 1 })).text, "In src/login.rs.");
        // A question's agent shares the main thread's folder: it is refused all of them.
        let theirs = call(cx, &repo, Some("q-1"), "wait_for", serde_json::json!({ "id": q }));
        assert!(theirs.error, "{}", theirs.text);
        // Not another's: a question's agent starts none.
        let refused = call(cx, &repo, Some("q-1"), "ask_agent", serde_json::json!({ "question": "x" }));
        assert!(refused.error && refused.text.contains("question's agent"), "{}", refused.text);
        let foreign = call(cx, &repo, None, "wait_for", serde_json::json!({ "id": q }));
        assert!(foreign.error, "an id of another task's agent: {}", foreign.text);

        // A task: the user is asked; declined, it is said.
        let proposed = call(cx, &parent, None, "start_task", serde_json::json!({ "title": "Try another way", "prompt": "Do it with a form." }));
        let declined = id_in(&proposed) as u32;
        assert_eq!(workspace.update(cx, |w, _| w.proposals().len()), 1, "it waits in Needs you");
        cx.update_window(handle.into(), |_, window, cx| {
            use gpui_kit::test::TestWindowExt as _;
            window.render_frame(cx);
            assert!(window.try_find(gpui_kit::SharedString::from(format!("proposal-{declined}"))).is_some(), "its row, with Start and Decline");
        })
        .unwrap();
        // The sidebar narrowed to a rail has it too.
        workspace.update(cx, |w, _| w.sidebar_rail = true);
        cx.update_window(handle.into(), |_, window, cx| {
            use gpui_kit::test::TestWindowExt as _;
            window.render_frame(cx);
            assert!(window.try_find(gpui_kit::SharedString::from(format!("rail-proposal-{declined}"))).is_some(), "on the rail");
        })
        .unwrap();
        workspace.update(cx, |w, _| w.sidebar_rail = false);
        step(cx, &mut |w, window, cx| w.answer_proposal(declined, false, window, cx));
        assert_eq!(call(cx, &parent, None, "wait_for", serde_json::json!({ "id": declined })).text, "The user declined it.");
        // Allowed: made out of view, its agent sent the prompt, marked whose.
        let proposed = call(cx, &parent, None, "start_task", serde_json::json!({ "title": "Try another way", "prompt": "Do it with a form." }));
        let allowed = id_in(&proposed) as u32;
        step(cx, &mut |w, window, cx| w.answer_proposal(allowed, true, window, cx));
        let mut child = None;
        wait(cx, "the task made", &mut |cx| {
            child = workspace.update(cx, |w, _| w.projects[0].tasks.iter().find(|t| t.spawned_by.as_deref() == Some(parent.as_path()) && t.busy.is_none() && t.view.is_some()).map(|t| t.path.clone()));
            child.is_some()
        });
        let child = child.unwrap();
        wait(cx, "its agent", &mut |_| child.join(".told-").exists());
        up(cx, &child, None);
        wait(cx, "its agent told", &mut |_| std::fs::read_to_string(child.join(".told-")).is_ok_and(|t| t.contains("Do it with a form.")));
        assert_eq!(workspace.update(cx, |w, _| w.active.clone()), Some(parent.clone()), "what you look at stays");
        // Its work, as the agent that asked for it reads it.
        std::fs::write(child.join("form.txt"), "a form\n").unwrap();
        let changes = call(cx, &parent, None, "agent_changes", serde_json::json!({ "id": allowed }));
        assert!(!changes.error && changes.text.contains("Commits:\nnone") && changes.text.contains("form.txt"), "{}", changes.text);
        // Kept in the state: after a restart the same ids are known.
        workspace.update(cx, |w, _| {
            let again = super::Crew::from_records(&w.crew.records());
            assert_eq!(again.records(), w.crew.records());
            assert_eq!(again.records().len(), 3);
        });
        let more = call(cx, &child, None, "start_task", serde_json::json!({ "title": "More", "prompt": "x" }));
        assert!(more.error && more.text.contains("started by another agent"), "{}", more.text);
        // Turned off in Settings: refused, even to an agent whose server still lists the tools.
        state::set_settings(state::Settings { agents_start_agents: false, ..state::settings() });
        let off = call(cx, &parent, None, "ask_agent", serde_json::json!({ "question": "x" }));
        assert!(off.error && off.text.contains("not allowed"), "{}", off.text);
        assert!(workspace.update(cx, |w, cx| w.snapshot(cx).projects[0].tasks.iter().any(|t| t.spawned_by.as_deref() == Some(parent.as_path()))), "kept in the state");

        step(cx, &mut |w, _, cx| {
            for view in w.views() {
                view.update(cx, |t, cx| t.shut_down_at_quit(cx));
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
