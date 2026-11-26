use super::*;

/// Which of the task's agents: its own, or one asked a question aside.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Who {
    Main,
    Question(u32),
}

impl Who {
    /// As the agent's environment and its reports carry it: none for the
    /// task's own agent, which is what reports of before say too.
    pub fn tag(self) -> Option<String> {
        match self {
            Who::Main => None,
            Who::Question(n) => Some(format!("q-{n}")),
        }
    }

    pub fn of_tag(tag: Option<&str>) -> Who {
        tag.and_then(|t| t.strip_prefix("q-")?.parse().ok()).map_or(Who::Main, Who::Question)
    }
}

/// What the task knows of one of its agents: what it was last reported
/// doing, what it said, what waits to be sent to it.
pub struct AgentState {
    pub status: crate::agent::AgentStatus,
    /// What it does now, at work: its latest tool call, in words.
    pub doing: Option<String>,
    /// The last words of its last turn, and what it waits on now.
    pub said: Option<crate::said::Said>,
    pub asks: Option<crate::said::Said>,
    /// What the latest tool call would do, for when it is asked about.
    pending: Option<String>,
    /// The status was guessed from the terminal, not reported.
    pub guessed: bool,
    /// What starts it, for Restart: its profile and session.
    pub launch: Option<crate::agents::Launch>,
    /// What tells, for an agent that reports no start of work, when it
    /// works and when it went quiet (activity.rs).
    pub(super) watch: activity::Watch,
    /// Text for its prompt once it is ready to take it, and whether it is
    /// sent too.
    pub(super) pending_prompt: Option<(String, bool)>,
    /// A message for it while busy: sent when its turn ends.
    queued: Option<String>,
    /// What a chat agent was last reported doing, not to report it twice.
    told: Option<(String, Option<String>)>,
    /// When it last reported anything.
    pub heard_at: Option<std::time::Instant>,
    /// The tool calls of its turn so far, the latest last, a few.
    calls: std::collections::VecDeque<String>,
    /// It makes the same call over and over in this turn: what it does.
    pub looping: Option<String>,
    /// You said to let it go on: not told again this turn.
    loop_allowed: bool,
    /// It set itself a timer (a cron, a wakeup): stopped, it would not go off.
    pub(super) timers: bool,
}

impl AgentState {
    pub fn new(launch: Option<crate::agents::Launch>) -> Self {
        Self {
            status: crate::agent::AgentStatus::Unknown,
            doing: None,
            said: None,
            asks: None,
            pending: None,
            guessed: false,
            launch,
            watch: activity::Watch::default(),
            pending_prompt: None,
            queued: None,
            told: None,
            heard_at: None,
            calls: Default::default(),
            looping: None,
            loop_allowed: false,
            timers: false,
        }
    }

    /// Keeps what the agent said: a turn's last words until the next
    /// prompt, a question until it is answered. A report without words (the
    /// reminder after a turn) leaves them as they were.
    /// True when it has just begun to go round in circles.
    pub fn hear(&mut self, event: &crate::agent::HookEvent, status: &crate::agent::AgentStatus) -> bool {
        use crate::said::Said;
        self.heard_at = Some(std::time::Instant::now());
        let was_looping = self.looping.is_some();
        if let Some(call) = &event.call {
            if matches!(event.tool(), Some("CronCreate" | "ScheduleWakeup")) {
                self.timers = true;
            }
            if self.calls.len() == LOOP_WINDOW {
                self.calls.pop_front();
            }
            self.calls.push_back(call.clone());
            if self.looping.is_none() && !self.loop_allowed && goes_round(self.calls.make_contiguous()) {
                self.looping = Some(event.message.clone().or_else(|| event.tool().map(str::to_string)).unwrap_or_default());
            }
        }
        match (event.event.as_str(), status) {
            ("SessionStart" | "UserPromptSubmit", _) => {
                self.said = None;
                self.asks = None;
                self.pending = None;
                self.calls.clear();
                self.looping = None;
                self.loop_allowed = false;
            }
            // A question or a plan, which waits for the user.
            ("PreToolUse", crate::agent::AgentStatus::NeedsInput(_)) => {
                self.asks = event.said.as_deref().and_then(Said::new);
                self.pending = None;
            }
            ("PostToolUse", _) => self.asks = None,
            ("PreToolUse", _) => {
                self.asks = None;
                self.pending = event.said.clone().or_else(|| event.message.clone());
            }
            ("Stop", _) => {
                self.asks = None;
                self.calls.clear();
                self.looping = None;
                self.loop_allowed = false;
                if let Some(said) = event.said.as_deref().and_then(Said::new) {
                    self.said = Some(said);
                }
            }
            ("Notification", crate::agent::AgentStatus::NeedsInput(_)) => {
                // After a question's own PreToolUse, its words are kept.
                if let Some(asks) = event.said.clone().or_else(|| self.pending.take()) {
                    self.asks = Said::new(&asks);
                }
            }
            ("Notification", crate::agent::AgentStatus::Done) if self.said.is_none() => {
                self.said = event.said.as_deref().and_then(Said::new);
            }
            _ => {}
        }
        !was_looping && self.looping.is_some()
    }

    /// What it said, as the row shows it: the question while it asks one.
    pub fn shown_said(&self) -> Option<&crate::said::Said> {
        match self.status {
            crate::agent::AgentStatus::NeedsInput(_) => self.asks.as_ref(),
            _ => self.said.as_ref(),
        }
    }

    pub fn has_queued_message(&self) -> bool {
        self.queued.is_some()
    }

    /// The agent said which session it runs: a restart resumes that one.
    pub fn learned_session(&mut self, id: &str) {
        if let Some(launch) = &mut self.launch {
            launch.learned(id);
        }
    }

    /// The session it runs, to find its file by.
    pub fn session_ref(&self) -> Option<crate::agents::SessionRef> {
        let launch = self.launch.as_ref()?;
        Some(crate::agents::SessionRef { profile: launch.profile.clone(), id: launch.session_id() })
    }

    /// Whether it is one that says when it is up: Claude Code, started with
    /// the hooks.
    fn reports(&self) -> bool {
        self.launch.as_ref().is_some_and(|l| l.profile.adapter().reports_ready() && l.hooks().is_some())
    }
}

/// The calls looked at for going round in circles.
const LOOP_WINDOW: usize = 6;

/// The latest calls are one made four times running, or two in turn three
/// times each: an agent that goes round in circles. A test run between
/// edits is not: the edits differ.
fn goes_round(calls: &[String]) -> bool {
    let n = calls.len();
    let same = n >= 4 && calls[n - 4..].iter().all(|c| *c == calls[n - 1]);
    let pair = n >= 6 && {
        let w = &calls[n - 6..];
        w[0] != w[1] && (2..6).all(|i| w[i] == w[i % 2])
    };
    same || pair
}

/// An agent asked a question aside, in the task's folder.
pub struct Question {
    pub id: u32,
    pub title: SharedString,
    pub agent: AgentState,
    /// Its tab while hidden (its ×): the agent goes on in it.
    parked: Option<TabItem>,
    /// Since when its agent has had nothing to do; none while it works or asks.
    pub(super) quiet_since: Option<std::time::Instant>,
    /// Its agent stopped for being idle: it resumes when shown.
    pub asleep: bool,
    /// It has messages: an agent that does not find them has lost them.
    talked: bool,
    /// How full its context is, 0 to 1.
    pub(super) context_fill: f32,
    /// Being looked for, before it starts again.
    waking: bool,
    /// Its conversation was not found when it woke: it started over.
    lost: bool,
}

impl Question {
    pub(super) fn has_parked(&self) -> bool {
        self.parked.is_some()
    }
}

/// A question's agent's names in the daemon: as a terminal, as a chat.
fn question_sessions(root: &std::path::Path, id: u32) -> [String; 2] {
    [format!("{}#q-{id}", root.display()), format!("{}#acp-q-{id}", root.display())]
}

/// Ends these sessions in the daemon, if they run there.
pub(super) fn end_sessions(names: &[String]) {
    #[cfg(unix)]
    for name in names {
        if let Some(daemon) = crate::daemon::client::for_session(name) {
            daemon.kill(name);
        }
    }
    #[cfg(not(unix))]
    let _ = names;
}

/// What the task asks of the workspace about its questions.
pub enum Asked {
    /// Closed from its bar: its agent ended, it goes to the history.
    Closed(u32),
    /// To go on as a task of its own.
    ToTask(u32),
    /// Its tab came to the front: its answer is read.
    Seen(u32),
}

// ---------------------------------------------------------------- questions

impl TaskView {
    /// A question asked now: its agent starts in a tab of its own, beside
    /// the task's, and is given `prompt`.
    pub fn ask(&mut self, id: u32, title: SharedString, launch: crate::agents::Launch, prompt: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.add_question(id, title, launch);
        self.show_question(id, window, cx);
        if let Some(prompt) = prompt {
            self.queue_prompt_to(Who::Question(id), prompt, true, window, cx);
        }
    }

    /// A question of before: listed, its agent started when it is shown.
    pub fn add_question(&mut self, id: u32, title: SharedString, launch: crate::agents::Launch) {
        if self.questions.iter().any(|q| q.id == id) {
            return;
        }
        self.questions.push(Question { id, title, agent: AgentState::new(Some(launch)), parked: None, quiet_since: Some(std::time::Instant::now()), asleep: false, talked: false, context_fill: 0., waking: false, lost: false });
    }

    /// The question's tab in front, its agent running. One asleep whose
    /// conversation had messages is looked for first: an agent that does not
    /// find it starts an empty one, which is to be said.
    pub fn show_question(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(q) = self.questions.iter_mut().find(|q| q.id == id) else { return };
        let look = (q.asleep && q.talked).then(|| q.agent.session_ref()).flatten().filter(|who| who.id.is_some() && !who.profile.acp && who.profile.adapter().session_base(&who.profile, &self.root).is_some());
        let Some(who) = look else { return self.open_question(id, window, cx) };
        if std::mem::replace(&mut q.waking, true) {
            return;
        }
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let found = cx.background_spawn(async move { who.own_transcript(&root).is_some_and(|file| file.is_file()) }).await;
            let _ = this.update_in(cx, |t, window, cx| {
                if let Some(q) = t.questions.iter_mut().find(|q| q.id == id) {
                    q.waking = false;
                    q.lost = !found;
                    t.open_question(id, window, cx);
                }
            });
        })
        .detach();
    }

    fn open_question(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.question_tab(id, window, cx) {
            self.activate(ix, window, cx);
            cx.emit(TaskEvent::Changed);
        }
    }

    /// Its tab with its agent in it: where it stood, else after the agents'.
    fn question_tab(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        let who = Who::Question(id);
        if let Some(ix) = self.agent_ix(who) {
            return Some(ix);
        }
        let q = self.questions.iter_mut().find(|q| q.id == id)?;
        let title = q.title.clone();
        q.asleep = false;
        q.quiet_since = Some(std::time::Instant::now());
        let tab = match q.parked.take() {
            Some(tab) => tab,
            None => {
                let launch = q.agent.launch.clone()?;
                let mut tab = self.agent_tab(who, &launch, self.terminal_env(), window, cx);
                tab.title = format!("? {}", crate::said::cap(&title, 40)).into();
                self.watch_activity(who, cx);
                tab
            }
        };
        if let Some(ix) = self.asleep_ix(who) {
            let (id, pane) = (self.tabs[ix].id, self.tabs[ix].pane);
            self.tabs[ix] = TabItem { id, pane, ..tab };
            return Some(ix);
        }
        let at = self.tabs.iter().rposition(|t| t.agent.is_some()).map_or(0, |ix| ix + 1);
        let pane = self.current;
        self.tabs.insert(at, TabItem { pane, ..tab });
        Some(at)
    }

    /// The tab holding an agent's place while it does not run.
    pub(super) fn asleep_ix(&self, who: Who) -> Option<usize> {
        self.tabs.iter().position(|t| t.agent == Some(who) && matches!(t.kind, TabKind::Asleep))
    }

    /// A question's tab of the last run: its place, its agent not started.
    pub(super) fn restore_question_tab(&mut self, id: u32) {
        let who = Who::Question(id);
        let Some(q) = self.questions.iter().find(|q| q.id == id) else { return };
        if self.asleep_ix(who).is_none() && self.agent_ix(who).is_none() {
            let title: SharedString = format!("? {}", crate::said::cap(&q.title, 40)).into();
            self.tabs.push(TabItem { agent: Some(who), ..TabItem::new(title, TabKind::Asleep) });
        }
    }

    /// Those in front that may run on in the daemon, attached to.
    pub(super) fn attach_questions_in_front(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let front: Vec<u32> = self.active.iter().flatten().filter_map(|id| self.tabs.iter().find(|t| t.id == *id)).filter(|t| matches!(t.kind, TabKind::Asleep)).filter_map(|t| match t.agent {
            Some(Who::Question(n)) => Some(n),
            _ => None,
        }).filter(|n| !self.question_asleep(*n)).collect();
        for n in front {
            self.question_tab(n, window, cx);
        }
    }

    /// Those whose tab is in front of a pane.
    pub fn questions_in_front(&self) -> Vec<u32> {
        self.active.iter().flatten().filter_map(|id| self.tabs.iter().find(|t| t.id == *id)).filter_map(|t| match t.agent {
            Some(Who::Question(n)) => Some(n),
            _ => None,
        }).collect()
    }

    #[cfg(all(test, unix))]
    pub fn question_wrote(&self, path: &str) -> bool {
        self.question_touched.contains_key(path)
    }

    #[cfg(all(test, unix))]
    pub fn tab_id(&self, ix: usize) -> usize {
        self.tabs[ix].id
    }

    /// Who each tab is the agent of, and whether it is only its place.
    #[cfg(all(test, unix))]
    pub fn agent_tabs(&self) -> Vec<(Who, bool)> {
        self.tabs.iter().filter_map(|t| t.agent.map(|who| (who, matches!(t.kind, TabKind::Asleep)))).collect()
    }

    /// Its own agent has been started, or it has none to start.
    pub fn runs_own_agent(&self) -> bool {
        self.agent.launch.is_none() || self.agent_ix(Who::Main).is_some()
    }

    /// A question's agent is not running: told at once, not looked for.
    pub fn question_sleeps(&mut self, id: u32) {
        if let Some(q) = self.questions.iter_mut().find(|q| q.id == id) {
            q.asleep = true;
        }
    }

    pub fn set_question_fill(&mut self, id: u32, fill: f32, cx: &mut Context<Self>) {
        if let Some(q) = self.questions.iter_mut().find(|q| q.id == id).filter(|q| q.context_fill != fill) {
            q.context_fill = fill;
            cx.notify();
        }
    }

    /// It has had a conversation to go on with.
    pub fn question_talked(&mut self, id: u32) {
        if let Some(q) = self.questions.iter_mut().find(|q| q.id == id) {
            q.talked = true;
        }
    }

    /// A branch change under a question's agent at work is asked about: it
    /// would go on reading another version of the files.
    pub(super) fn mind_questions(&mut self, act: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static, window: &mut Window, cx: &mut Context<Self>) {
        if !self.question_busy() {
            return act(self, window, cx);
        }
        let (this, act) = (cx.weak_entity(), Rc::new(std::cell::RefCell::new(Some(act))));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (this, act) = (this.clone(), act.clone());
            dialog
                .confirm()
                .title("Change the branch?")
                .description("A question's agent is at work in this folder and would go on with the files of the other branch.")
                .ok_text("Change it")
                .on_ok(move |_, window, cx| {
                    if let Some(act) = act.borrow_mut().take() {
                        let _ = this.update(cx, |t, cx| act(t, window, cx));
                    }
                    true
                })
        });
    }

    /// What stands in a tab whose agent is not running.
    pub(super) fn render_asleep(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let who = self.tabs[ix].agent;
        if who == Some(Who::Main) && self.agent_waits_for_chrome {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("The agent starts once its host reaches Chrome here (Settings → Browser)…")
                .into_any_element();
        }
        let (text, label) = match who {
            Some(Who::Question(_)) => ("This question's agent is asleep. Its conversation goes on when it starts.", "Continue"),
            _ if self.agent_slept => ("The agent was stopped after it had nothing to do for a while. Its conversation goes on when it starts.", "Continue"),
            _ => ("The agent is paused. Its conversation goes on when it starts.", "Start the agent"),
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .child(Button::new("asleep-start").primary().small().label(label).on_click(cx.listener(move |this, _, window, cx| match who {
                Some(Who::Question(n)) => this.show_question(n, window, cx),
                _ => this.show_view(0, window, cx),
            })))
            .into_any_element()
    }

    /// Its tab out of sight; the question stays open, its agent at work.
    pub fn hide_question(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.agent_ix(Who::Question(id)) else { return };
        let tab = self.take_tab(ix, window, cx);
        if let Some(q) = self.questions.iter_mut().find(|q| q.id == id) {
            q.parked = Some(tab);
        }
        cx.emit(TaskEvent::Changed);
    }

    /// The first of its questions that waits for an answer.
    pub fn question_asks(&self) -> Option<u32> {
        self.questions.iter().find(|q| matches!(q.agent.status, crate::agent::AgentStatus::NeedsInput(_))).map(|q| q.id)
    }

    /// Questions whose agent has had nothing to do for `idle`, out of
    /// sight, stop it: their conversation resumes when they are shown.
    /// Not one that asks something, which would be lost, nor one whose
    /// session is not known, which could not be resumed.
    pub fn sleep_idle_questions(&mut self, idle: Duration, cx: &mut Context<Self>) {
        use crate::agent::AgentStatus::*;
        let in_front: Vec<usize> = self.active.iter().flatten().copied().collect();
        let sleepy: Vec<u32> = self
            .questions
            .iter()
            .filter(|q| !q.asleep && q.quiet_since.is_some_and(|at| at.elapsed() >= idle))
            .filter(|q| matches!(q.agent.status, Unknown | Idle | Done | Exited))
            .filter(|q| q.agent.launch.as_ref().is_some_and(|l| l.session_id().is_some()))
            .filter(|q| self.agent_ix(Who::Question(q.id)).is_none_or(|ix| !in_front.contains(&self.tabs[ix].id)))
            .map(|q| q.id)
            .collect();
        for id in sleepy {
            // Its tab stays where it is, holding its place.
            let tab = match self.agent_ix(Who::Question(id)) {
                Some(ix) => {
                    let (tab_id, pane, title) = (self.tabs[ix].id, self.tabs[ix].pane, self.tabs[ix].title.clone());
                    let asleep = TabItem { id: tab_id, pane, agent: Some(Who::Question(id)), ..TabItem::new(title, TabKind::Asleep) };
                    Some(std::mem::replace(&mut self.tabs[ix], asleep))
                }
                None => self.questions.iter_mut().find(|q| q.id == id).and_then(|q| q.parked.take()),
            };
            match tab.map(|t| t.kind) {
                Some(TabKind::Terminal(view)) => TermView::end(&view, cx),
                Some(TabKind::Chat(chat)) => chat.update(cx, |c, _| c.end()),
                // Not shown since a restart: it may run on in the daemon.
                _ => end_sessions(&question_sessions(&self.root, id)),
            }
            if let Some(q) = self.questions.iter_mut().find(|q| q.id == id) {
                q.asleep = true;
                q.agent.status = crate::agent::AgentStatus::Unknown;
                q.agent.watch = activity::Watch::default();
                q.agent.launch = q.agent.launch.as_ref().map(|l| l.again());
            }
            cx.emit(TaskEvent::Changed);
        }
    }

    pub fn question_asleep(&self, id: u32) -> bool {
        self.questions.iter().any(|q| q.id == id && q.asleep)
    }

    /// Where the agents of open questions not shown since a restart may run
    /// on, in the daemon: a tab made for one attaches to it there.
    pub(super) fn unshown_sessions(&self) -> impl Iterator<Item = String> + '_ {
        self.questions.iter().filter(|q| !q.asleep && q.parked.is_none() && self.agent_ix(Who::Question(q.id)).is_none()).flat_map(|q| question_sessions(&self.root, q.id))
    }

    /// The question is done with: its agent ends, its tab goes.
    pub fn end_question(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        if self.questions.iter().any(|q| q.id == id && q.parked.is_none() && !q.asleep) && self.agent_ix(Who::Question(id)).is_none() {
            end_sessions(&question_sessions(&self.root, id));
        }
        let Some(pos) = self.questions.iter().position(|q| q.id == id) else { return };
        let mut question = self.questions.remove(pos);
        self.forget_written(id);
        if let Some(ix) = self.asleep_ix(Who::Question(id)) {
            self.remove_tab(ix, window, cx);
        }
        let tab = match self.agent_ix(Who::Question(id)) {
            Some(ix) => Some(self.take_tab(ix, window, cx)),
            None => question.parked.take(),
        };
        match tab.map(|t| t.kind) {
            Some(TabKind::Terminal(view)) => TermView::end(&view, cx),
            Some(TabKind::Chat(chat)) => chat.update(cx, |c, _| c.end()),
            _ => {}
        }
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    pub fn rename_question(&mut self, id: u32, title: SharedString, cx: &mut Context<Self>) {
        let short: SharedString = format!("? {}", crate::said::cap(&title, 40)).into();
        if let Some(q) = self.questions.iter_mut().find(|q| q.id == id) {
            q.title = title;
            if let Some(tab) = &mut q.parked {
                tab.title = short.clone();
            }
        }
        if let Some(ix) = self.agent_ix(Who::Question(id)).or_else(|| self.asleep_ix(Who::Question(id))) {
            self.tabs[ix].title = short;
        }
        cx.notify();
    }

    /// The task's agent, started late: a question asked of a task not
    /// running started only the question's.
    pub(super) fn start_main_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_ix(Who::Main).is_some() {
            return;
        }
        let Some(launch) = self.agent.launch.clone() else { return };
        self.agent_slept = false;
        self.agent.status = crate::agent::AgentStatus::Unknown;
        self.agent.guessed = false;
        // A new process: what the old one set to go off went with it.
        self.agent.timers = false;
        let tab = self.agent_tab(Who::Main, &launch, self.terminal_env(), window, cx);
        // In the shell's place, which stood for it.
        match self.tabs.iter().position(|t| (t.title == "shell" && t.agent.is_none()) || (t.agent == Some(Who::Main) && matches!(t.kind, TabKind::Asleep))) {
            Some(ix) => {
                if let TabKind::Terminal(old) = &self.tabs[ix].kind {
                    TermView::end(old, cx);
                }
                let id = self.tabs[ix].id;
                self.tabs[ix] = TabItem { id, pane: self.tabs[ix].pane, ..tab };
            }
            None => self.tabs.insert(0, tab),
        }
        self.watch_activity(Who::Main, cx);
        cx.emit(TaskEvent::Changed);
    }

    /// Above the tab of an agent that goes round in circles: what it
    /// repeats, its turn stopped (Esc) or let go on.
    pub(super) fn render_loop_bar(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let who = self.tabs.get(ix)?.agent?;
        let doing = self.agent_of(who)?.looping.clone()?;
        Some(
            h_flex()
                .flex_none()
                .gap_2()
                .px_3()
                .py_1()
                .text_xs()
                .bg(cx.theme().warning.opacity(0.15))
                .border_b_1()
                .border_color(cx.theme().border)
                .child(div().flex_1().min_w_0().truncate().child(format!("↻ It goes round in circles: {doing}")))
                .child(
                    Button::new("loop-stop")
                        .xsmall()
                        .outline()
                        .label("Stop its turn")
                        .tooltip("As Esc in its terminal: it stops and waits for you")
                        .on_click(cx.listener(move |this, _, window, cx| this.stop_turn(who, window, cx))),
                )
                .child(
                    Button::new("loop-ok")
                        .xsmall()
                        .ghost()
                        .label("Let it go on")
                        .tooltip("Not told again this turn")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(agent) = this.agent_of_mut(who) {
                                agent.looping = None;
                                agent.loop_allowed = true;
                            }
                            cx.emit(TaskEvent::Changed);
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// The agent's turn stopped, as Esc does.
    fn stop_turn(&mut self, who: Who, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(chat) = self.agent_chat(who) {
            chat.update(cx, |c, _| c.send(crate::acp::wire::ToHost::Cancel));
        } else if let Some(state) = self.agent_terminal(who, cx) {
            state.update(cx, |s, cx| s.send_text("\u{1b}", cx));
        }
        if let Some(agent) = self.agent_of_mut(who) {
            agent.looping = None;
            agent.loop_allowed = true;
        }
        if let Some(ix) = self.agent_ix(who) {
            self.activate(ix, window, cx);
        }
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// Above a question's tab: what it is, and its ends.
    pub(super) fn render_question_bar(&self, ix: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Who::Question(id)) = self.tabs.get(ix)?.agent else { return None };
        let question = self.questions.iter().find(|q| q.id == id)?;
        let (title, lost) = (question.title.clone(), question.lost);
        Some(
            h_flex()
                .flex_none()
                .gap_2()
                .px_3()
                .py_1()
                .text_xs()
                .bg(cx.theme().muted)
                .border_b_1()
                .border_color(cx.theme().border)
                .child(svg().path("side-icons/message-circle-question-mark.svg").size(px(14.)).flex_none().text_color(cx.theme().muted_foreground))
                .child(div().flex_1().min_w_0().truncate().text_color(cx.theme().muted_foreground).child(crate::copying::text("question-title", title)))
                .when(lost, |bar| bar.child(div().flex_none().text_color(cx.theme().warning).child("Its earlier conversation was not found: this one starts over")))
                .child(
                    Button::new("question-to-task")
                        .xsmall()
                        .ghost()
                        .label("Make a task")
                        .tooltip("Go on with it in a task of its own: a worktree and a branch, its conversation carried there, and the files it wrote moved by its agent")
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(TaskEvent::Asked(Asked::ToTask(id))))),
                )
                .child(
                    Button::new("question-close")
                        .xsmall()
                        .outline()
                        .label("Close question")
                        .tooltip("End its agent; the question goes to the project's closed questions")
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(TaskEvent::Asked(Asked::Closed(id))))),
                )
                .into_any_element(),
        )
    }
}

impl TaskView {
    pub fn agent_of(&self, who: Who) -> Option<&AgentState> {
        match who {
            Who::Main => Some(&self.agent),
            Who::Question(n) => self.questions.iter().find(|q| q.id == n).map(|q| &q.agent),
        }
    }

    pub fn agent_of_mut(&mut self, who: Who) -> Option<&mut AgentState> {
        match who {
            Who::Main => Some(&mut self.agent),
            Who::Question(n) => self.questions.iter_mut().find(|q| q.id == n).map(|q| &mut q.agent),
        }
    }

    /// The tab an agent runs in: its terminal or its chat.
    pub(super) fn agent_ix(&self, who: Who) -> Option<usize> {
        self.tabs.iter().position(|t| t.agent == Some(who) && matches!(t.kind, TabKind::Terminal(_) | TabKind::Chat(_)))
    }

    /// The agent whose terminal `state` is.
    pub(super) fn agent_of_terminal(&self, state: &Entity<TerminalState>, cx: &App) -> Option<Who> {
        self.tabs.iter().find_map(|t| match &t.kind {
            TabKind::Terminal(v) if v.read(cx).state == *state => t.agent,
            _ => None,
        })
    }

    /// The tab of an agent, as it is started: a chat for one that speaks
    /// ACP, else a terminal running its command.
    pub(super) fn agent_tab(&self, who: Who, launch: &crate::agents::Launch, task_env: Vec<(String, String)>, window: &mut Window, cx: &mut Context<Self>) -> TabItem {
        Self::make_agent_tab(&self.root, self.hook_socket.as_ref(), who, launch, task_env, window, cx)
    }

    pub(super) fn make_agent_tab(root: &PathBuf, hook_socket: Option<&String>, who: Who, launch: &crate::agents::Launch, task_env: Vec<(String, String)>, window: &mut Window, cx: &mut Context<Self>) -> TabItem {
        if launch.profile.acp {
            return Self::make_chat(root, who, launch, task_env, window, cx);
        }
        let mut env = task_env;
        env.extend(launch.env(root));
        Self::make_terminal(root, hook_socket, Term::Agent(who), Some(launch.command()), env, None, window, cx)
    }

    /// Puts code into the agent's prompt, without sending it: the location,
    /// then the code fenced, so you can type the question after it.
    pub fn send_to_agent(&mut self, location: String, code: String, language: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = format!("{location}\n```{language}\n{}\n```\n", code.trim_end_matches('\n'));
        self.paste_to_agent(text, window, cx);
    }

    /// Starts the task's agent again in its tab.
    pub fn restart_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_ix(Who::Main).is_none() {
            return self.start_main_agent(window, cx);
        }
        self.restart(Who::Main, window, cx);
    }

    /// Starts the agent of this terminal again: the button of an agent
    /// that ended.
    pub(super) fn restart_in(&mut self, state: &Entity<TerminalState>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(who) = self.agent_of_terminal(state, cx) {
            self.restart(who, window, cx);
        }
    }

    /// Starts an agent again in its tab: typed into its shell, or in a new
    /// terminal when the shell is gone too.
    pub fn restart(&mut self, who: Who, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.agent_ix(who) else { return };
        if let TabKind::Chat(chat) = &self.tabs[ix].kind {
            chat.clone().update(cx, |c, cx| c.restart(cx));
            if let Some(agent) = self.agent_of_mut(who) {
                agent.told = None;
                agent.status = crate::agent::AgentStatus::Unknown;
            }
            self.activate(ix, window, cx);
            cx.emit(TaskEvent::Changed);
            return cx.notify();
        }
        let Some(mut launch) = self.agent_of(who).and_then(|a| a.launch.as_ref()).map(|l| l.again()) else { return };
        // The latest session in the folder may be a question's.
        if launch.start == crate::agents::Start::Last && self.asked {
            launch.start = crate::agents::Start::New(None);
        }
        let command = launch.command();
        let TabKind::Terminal(view) = &self.tabs[ix].kind else { return };
        let view = view.clone();
        let state = view.read(cx).state.clone();
        if TermView::gone(state.read(cx)) {
            TermView::end(&view, cx);
            let tab = self.agent_tab(who, &launch, self.terminal_env(), window, cx);
            (self.tabs[ix].kind, self.tabs[ix].shown) = (tab.kind, tab.shown);
        } else {
            let enter = if cfg!(windows) { "\r" } else { "\n" };
            view.update(cx, |v, _| v.ran = false);
            state.update(cx, |s, cx| s.send_text(format!("{command}{enter}"), cx));
        }
        if let Some(agent) = self.agent_of_mut(who) {
            agent.launch = Some(launch);
            agent.status = crate::agent::AgentStatus::Unknown;
            agent.guessed = false;
            agent.timers = false;
        }
        self.activate(ix, window, cx);
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// Another agent in the agent's tab: the one running ends with its
    /// terminal, the new one starts in a terminal of its own, with its
    /// own environment.
    pub fn switch_agent(&mut self, launch: crate::agents::Launch, then: Then, window: &mut Window, cx: &mut Context<Self>) {
        let task_env = self.terminal_env();
        // The conversation before was another agent's: not the new one's to go on with.
        if let Some(chat) = self.agent_chat(Who::Main) {
            chat.update(cx, |c, _| c.end());
        }
        // A new conversation: the old one's journal, log and checkpoints go,
        // (a remote task's on its host) before the new agent starts: its host
        // would find the old journal, and its first checkpoint could go with the old ones.
        let (old, threads) = crate::acp::journal_files(&self.root);
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_spawn(async move {
                for file in old {
                    let _ = crate::hostfs::remove(&file);
                }
                for thread in threads {
                    crate::acp::forget_checkpoints(&root, &thread);
                }
            })
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.put_agent(launch, task_env, window, cx);
                then(this, window, cx);
            });
        })
        .detach();
    }

    /// The new agent of [`Self::switch_agent`] in the agent's tab.
    fn put_agent(&mut self, launch: crate::agents::Launch, task_env: Vec<(String, String)>, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.agent_tab(Who::Main, &launch, task_env, window, cx);
        let ix = match self.agent_ix(Who::Main) {
            Some(ix) => {
                if let TabKind::Terminal(old) = &self.tabs[ix].kind {
                    TermView::end(old, cx);
                }
                (self.tabs[ix].kind, self.tabs[ix].shown) = (tab.kind, tab.shown);
                ix
            }
            None => {
                self.tabs.insert(0, tab);
                0
            }
        };
        self.agent = AgentState::new(Some(launch));
        self.watch_activity(Who::Main, cx);
        self.activate(ix, window, cx);
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// The agent's mark, when there are several to tell apart.
    pub fn agent_letter(&self) -> Option<String> {
        let launch = self.agent.launch.as_ref()?;
        crate::state::with_settings(|s| s.several_agents()).then(|| launch.profile.letter())
    }

    /// Puts `text` into the task's agent's prompt once it is up.
    pub fn queue_prompt(&mut self, text: String, send: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.queue_prompt_to(Who::Main, text, send, window, cx);
    }

    /// Puts `text` into an agent's prompt once the agent is up: Claude Code
    /// says so with SessionStart; another agent gets a few seconds after it
    /// starts. Pasted before, the shell would read it.
    ///
    /// With `send` it is sent as well, not left for the user to finish.
    pub fn queue_prompt_to(&mut self, who: Who, text: String, send: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_of(who).is_none_or(|a| a.launch.is_none()) {
            return;
        }
        // Its host takes a prompt whenever it comes, and sends it when the
        // agent is up.
        if let Some(chat) = self.agent_chat(who) {
            chat.update(cx, |c, cx| c.put(text, send, window, cx));
            return;
        }
        if let Some(agent) = self.agent_of_mut(who) {
            agent.pending_prompt = Some((text, send));
        }
        cx.spawn_in(window, async move |this, cx| {
            let started = std::time::Instant::now();
            let mut running_since: Option<std::time::Instant> = None;
            // When its output last grew: an agent that tells nothing is
            // up once it has drawn its screen and gone quiet.
            let (mut output, mut wrote_at) = (0u64, std::time::Instant::now());
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let ready = this.update(cx, |t, cx| {
                    let agent = t.agent_of(who)?;
                    agent.pending_prompt.as_ref()?;
                    let (hooked, patient) = (!matches!(agent.status, crate::agent::AgentStatus::Unknown), agent.reports());
                    let state = t.agent_terminal(who, cx)?;
                    let state = state.read(cx);
                    let running = state.foreground().is_some_and(|p| !p.is_shell());
                    Some((hooked, patient, running, state.output_seen()))
                });
                let Ok(Some((hooked, patient, running, seen))) = ready else { return };
                if seen != output {
                    (output, wrote_at) = (seen, std::time::Instant::now());
                }
                if running {
                    running_since.get_or_insert_with(std::time::Instant::now);
                }
                let waited = running_since.map_or(Duration::ZERO, |at| at.elapsed());
                let settled = waited > Duration::from_millis(1500) && wrote_at.elapsed() > Duration::from_millis(1200);
                // Its input box takes a moment after the hook.
                // An agent that reports is waited for: until it says it
                // is up it may be asking something (to trust the folder),
                // and the prompt would go into that.
                if (hooked && waited > Duration::from_millis(500))
                    || (!patient && (settled || waited > Duration::from_secs(8) || started.elapsed() > Duration::from_secs(30)))
                    || started.elapsed() > Duration::from_secs(1800)
                {
                    break;
                }
            }
            let _ = this.update_in(cx, |t, window, cx| {
                let Some(agent) = t.agent_of_mut(who) else { return };
                // Sent only to an agent that said it is up. One that did not
                // may be asking something first (to trust the folder), and
                // Enter would answer that.
                let up = !matches!(agent.status, crate::agent::AgentStatus::Unknown) || !agent.reports();
                let pending = agent.pending_prompt.take();
                match pending {
                    Some((text, true)) if up => t.submit_to(who, text, window, cx),
                    Some((text, _)) => t.paste_to(who, text, window, cx),
                    None => {}
                }
            });
        })
        .detach();
    }

    /// Text into the task's agent's prompt, for the user to add to and send.
    pub fn paste_to_agent(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.paste_to(Who::Main, text, window, cx);
    }

    pub fn paste_to(&mut self, who: Who, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.agent_ix(who) else { return };
        match &self.tabs[ix].kind {
            TabKind::Chat(chat) => chat.clone().update(cx, |c, cx| c.put(text, false, window, cx)),
            TabKind::Terminal(view) => {
                let state = view.read(cx).state.clone();
                // Bracketed paste: Claude Code shows it as pasted text, not a prompt sent.
                state.update(cx, |s, cx| s.paste_text(text, cx));
            }
            _ => {}
        }
        self.activate(ix, window, cx);
    }

    /// Text into an agent's prompt, and sent: Enter a moment after the
    /// paste, which the agent would otherwise take for a line of it.
    pub fn submit_to(&mut self, who: Who, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(chat) = self.agent_chat(who) {
            chat.update(cx, |c, cx| c.put(text, true, window, cx));
            return;
        }
        self.paste_to(who, text, window, cx);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let _ = this.update(cx, |t, cx| {
                if let Some(state) = t.agent_terminal(who, cx) {
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
        match self.agent.status {
            Working | NeedsInput(_) => {
                self.agent.queued = Some(match self.agent.queued.take() {
                    Some(waiting) => format!("{waiting}\n\n{text}"),
                    None => text,
                });
                Told::Later
            }
            Idle | Done => {
                self.submit_to(Who::Main, text, window, cx);
                Told::Now
            }
            Unknown | Exited => Told::Not,
        }
    }

    /// An agent's terminal.
    pub(super) fn agent_terminal(&self, who: Who, cx: &App) -> Option<Entity<TerminalState>> {
        match &self.tabs[self.agent_ix(who)?].kind {
            TabKind::Terminal(view) => Some(view.read(cx).state.clone()),
            _ => None,
        }
    }

    pub fn has_queued_message(&self) -> bool {
        self.agent.has_queued_message()
    }

    /// An agent's turn ended: what waited for that goes out.
    pub fn deliver_queued(&mut self, who: Who, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.agent_of_mut(who).and_then(|a| a.queued.take()) {
            self.submit_to(who, text, window, cx);
        }
    }
}

// ---------------------------------------------------------------- chat agents

impl TaskView {
    /// The tab of an agent that speaks ACP: its host is the task's, in the
    /// daemon when there is one.
    pub(super) fn make_chat(root: &PathBuf, who: Who, launch: &crate::agents::Launch, task_env: Vec<(String, String)>, window: &mut Window, cx: &mut Context<Self>) -> TabItem {
        let spec = Self::chat_spec(root, who, launch, task_env);
        let id = match who.tag() {
            Some(tag) => format!("{}#acp-{tag}", root.display()),
            None => format!("{}#acp", root.display()),
        };
        let task = cx.weak_entity();
        let chat = cx.new(|cx| ChatView::new(id, spec, task, window, cx));
        let profile = launch.profile.id.clone();
        cx.subscribe_in(&chat, window, move |this: &mut Self, chat, event: &ChatEvent, window, cx| match event {
            ChatEvent::Follow { path, line } => this.follow_file(chat.clone(), path.clone(), *line, window, cx),
            ChatEvent::FollowChanged { since } => this.follow_changed(chat.clone(), *since, window, cx),
            ChatEvent::Changed => this.chat_changed(who, chat, cx),
            ChatEvent::Chose { id, value } => crate::state::update_profile(&profile, |p| p.choices.insert(id.clone(), value.clone()).as_ref() != Some(value)),
            ChatEvent::Offered => {
                let offered = chat.read(cx).thread.options.clone();
                crate::state::update_profile(&profile, |p| {
                    // The values to choose from, not what is chosen in this task.
                    let same = p.offered.len() == offered.len() && p.offered.iter().zip(&offered).all(|(a, b)| a.id == b.id && a.values == b.values);
                    if !same {
                        p.offered = offered;
                    }
                    !same
                });
            }
        })
        .detach();
        let title = if who == Who::Main { "agent" } else { "?" };
        TabItem { agent: Some(who), ..TabItem::new(title, TabKind::Chat(chat)) }
    }

    fn chat_spec(root: &PathBuf, who: Who, launch: &crate::agents::Launch, mut env: Vec<(String, String)>) -> crate::acp::wire::HostSpec {
        use crate::acp::policy::{CHOICE, Policy};
        env.extend(launch.profile.env_vars());
        env.push((crate::agent::ROLE_ENV.into(), "agent".into()));
        env.extend(who.tag().map(|tag| (crate::agent::AGENT_ENV.to_string(), tag)));
        // OpenCode asks nothing on its own: made to ask, the app's rules
        // answer what is safe (unless the profile configures it itself).
        let opencode = launch.profile.kind == crate::agents::Kind::OpenCode;
        if opencode && !env.iter().any(|(k, _)| k == "OPENCODE_CONFIG_CONTENT") {
            env.push(("OPENCODE_CONFIG_CONTENT".into(), r#"{"permission":{"edit":"ask","bash":"ask","webfetch":"ask"}}"#.into()));
        }
        let policy = match launch.profile.choices.get(CHOICE) {
            Some(chosen) => Policy::parse(chosen),
            None if opencode => Policy::Rules,
            None => Policy::Ask,
        };
        crate::acp::wire::HostSpec {
            command: launch.profile.command.clone(),
            cwd: root.clone(),
            env,
            // Only a session the agent made: an id the app chose for a terminal
            // agent's start is none it knows.
            resume: match &launch.start {
                crate::agents::Start::Resume(id) => Some(id.clone()),
                _ => None,
            },
            journal: Some(crate::acp::journal_of(root, who.tag().as_deref())),
            unasked: launch.unasked,
            // What was chosen last for this agent, then what this task asks.
            options: launch.profile.choices.iter().filter(|(k, _)| k.as_str() != CHOICE).map(|(k, v)| (k.clone(), v.clone())).chain(launch.options.iter().cloned()).collect::<BTreeMap<_, _>>().into_iter().collect(),
            policy: policy.id().to_string(),
            mcp: crate::mcp::server_for(root),
            unanswered_secs: crate::state::with_settings(|s| u64::from(s.agent_unanswered_minutes) * 60),
            notify_as: crate::state::with_settings(|s| s.notifications).then(|| launch.profile.name.clone()),
            budget: launch.profile.daily_limit.map(|limit| (limit - crate::acp::spend::spent_today(&launch.profile.id)).max(0.0)),
            // Set where it runs ([`crate::acp::link`]).
            login_shell: false,
            journal_before: Some(crate::acp::journal_of_before(root, who.tag().as_deref())),
        }
    }

    /// A chat agent asked to go on with a session: if it cannot, this is
    /// its first prompt.
    pub fn otherwise_tell(&self, text: String, cx: &mut Context<Self>) {
        if let Some(chat) = self.agent_chat(Who::Main) {
            chat.read(cx).send(crate::acp::wire::ToHost::Otherwise { prompt: text });
        }
    }

    /// An agent's chat, when it speaks ACP.
    pub(super) fn agent_chat(&self, who: Who) -> Option<Entity<ChatView>> {
        match &self.tabs[self.agent_ix(who)?].kind {
            TabKind::Chat(chat) => Some(chat.clone()),
            _ => None,
        }
    }

    /// The tabs, and those of questions hidden: what runs in the task.
    pub(super) fn all_tabs(&self) -> impl Iterator<Item = &TabItem> + '_ {
        self.tabs.iter().chain(self.questions.iter().filter_map(|q| q.parked.as_ref()))
    }

    /// The chats of all the task's agents, hidden ones too.
    pub(super) fn chats(&self) -> impl Iterator<Item = Entity<ChatView>> + '_ {
        self.all_tabs().filter_map(|tab| match &tab.kind {
            TabKind::Chat(chat) => Some(chat.clone()),
            _ => None,
        })
    }

    /// The chat agent's status, told as the hooks of a terminal agent are:
    /// the workspace notifies, marks the task and keeps the session.
    fn chat_changed(&mut self, who: Who, chat: &Entity<ChatView>, cx: &mut Context<Self>) {
        use crate::acp::thread::Status;
        let thread = &chat.read(cx).thread;
        let Some(profile) = self.agent_of(who).map(|a| a.launch.as_ref().map(|l| l.profile.id.clone())) else { return };
        if let Some(usage) = &thread.usage {
            if usage.size > 0 {
                let fill = (usage.used as f32 / usage.size as f32).min(1.);
                match who {
                    Who::Main => self.context_fill = fill,
                    Who::Question(n) => {
                        if let Some(q) = self.questions.iter_mut().find(|q| q.id == n) {
                            q.context_fill = fill;
                        }
                    }
                }
            }
            // What it cost, counted for its profile's day.
            if let (Some(cost), Some(session), Some(profile)) = (usage.cost.as_deref().and_then(crate::acp::spend::usd), &thread.session, &profile) {
                crate::acp::spend::record(profile, session, cost);
            }
        }
        let session = thread.session.clone();
        let (event, kind, message) = match &thread.status {
            Status::Starting => return,
            Status::Idle => ("SessionStart", None, None),
            Status::Working => ("UserPromptSubmit", None, None),
            Status::Asking => {
                let what = thread.asking().map(|t| if t.title.is_empty() { t.kind.clone() } else { t.title.clone() });
                ("Notification", Some("permission_prompt".to_string()), Some(what.map_or_else(|| "The agent asks for permission".into(), |w| format!("Allow: {w}?"))))
            }
            Status::Done => ("Stop", None, None),
            Status::SignIn => ("Notification", Some("auth".to_string()), Some("The agent wants you to sign in".to_string())),
            Status::Exited(_) => ("SessionEnd", None, None),
        };
        let told = (event.to_string(), message.clone());
        let said = thread.last_said().map(|s| s.to_string());
        let Some(agent) = self.agent_of_mut(who) else { return };
        if let Some(id) = &session {
            agent.learned_session(id);
        }
        if agent.told.as_ref() == Some(&told) {
            return;
        }
        agent.told = Some(told);
        cx.emit(TaskEvent::Heard(crate::agent::HookEvent {
            task: self.root.clone(),
            agent: who.tag(),
            event: event.into(),
            message,
            kind,
            file: None,
            said: said.filter(|_| event == "Stop" || event == "Notification"),
            replayed: None,
            session,
            guessed: false,
            worktree: None,
            call: None,
        }));
        cx.notify();
    }

    /// A file the followed agent reads or changes, in the pane the chat is
    /// not in (split first if need be), the keyboard left with the chat.
    fn follow_file(&mut self, chat: Entity<ChatView>, path: PathBuf, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.iter().any(|t| matches!(&t.kind, TabKind::Chat(c) if *c == chat)) {
            return;
        }
        self.open_file_as(path.clone(), line.saturating_sub(1).max(0), 0, OpenAs::Preview, window, cx);
        let opened = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::File(f) if f.read(cx).path == path));
        if let Some(ix) = opened {
            self.beside_chat(&chat, ix, window, cx);
        }
    }

    /// The agent ran a command that named no file: of the files the task
    /// has changed, the one written last since the command began is shown,
    /// at its first change. Asked of the disk after the refresh the change
    /// brought, and off the UI thread.
    fn follow_changed(&mut self, chat: Entity<ChatView>, since: std::time::SystemTime, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.refresh_tx.try_send(());
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(600)).await;
            let Ok(changed) = this.update(cx, |this, _| this.files.iter().take(500).map(|file| file.path.to_string()).collect::<Vec<_>>()) else { return };
            let newest = cx
                .background_spawn({
                    let root = root.clone();
                    async move { last_written(&root, &changed, since) }
                })
                .await;
            let Some(path) = newest else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                let line = this.uncommitted_lines.get(&path).copied().unwrap_or(1);
                this.follow_file(chat, root.join(&path), line, window, cx);
            });
        })
        .detach();
    }

    /// Tab `ix` in the other column than the chat's (made if need be), the
    /// chat in front in its own pane, keeping the keyboard.
    pub(super) fn beside_chat(&mut self, chat: &Entity<ChatView>, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let chat_ix = || self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Chat(c) if c == chat));
        let Some(chat_pane) = chat_ix().map(|i| self.tabs[i].pane) else { return };
        if self.tabs.get(ix).is_some_and(|t| t.pane.right == chat_pane.right) {
            self.move_to(ix, Pane { right: !chat_pane.right, below: false }, window, cx);
        }
        if let Some(chat_ix) = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Chat(c) if c == chat)) {
            self.activate(chat_ix, window, cx);
        }
    }

    /// Chat agents with nothing to do stop; their conversations stay.
    pub fn put_agent_to_sleep(&mut self, cx: &mut Context<Self>) {
        for chat in self.chats().collect::<Vec<_>>() {
            chat.update(cx, |c, cx| c.sleep(cx));
        }
        self.sleep_terminal_agent(cx);
    }

    pub fn wake_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for chat in self.chats().collect::<Vec<_>>() {
            chat.update(cx, |c, cx| c.wake(cx));
        }
        if self.agent_slept {
            self.start_main_agent(window, cx);
        }
    }

    /// Stopped for having nothing to do; it resumes when shown.
    pub fn agent_slept(&self) -> bool {
        self.agent_slept
    }

    pub fn set_agent_slept(&mut self, slept: bool) {
        self.agent_slept = slept && self.agent_ix(Who::Main).is_none();
    }

    /// The task's agent in a terminal, stopped as a chat's is (Settings):
    /// only one that says what it does and whose session is known, which
    /// resumes as it was; not while it works or asks, nor while something of
    /// its own would end with it: a message for it, text typed since it last
    /// spoke and maybe not sent, a timer it set, a shell it runs (a command
    /// in the background), looked for off the UI thread.
    fn sleep_terminal_agent(&mut self, cx: &mut Context<Self>) {
        use crate::agent::AgentStatus::*;
        let probed = self.agent_sleep_probe.is_some_and(|at| at.elapsed() < Duration::from_secs(60));
        if probed || self.visible || !crate::state::with_settings(|s| s.sleep_terminal_agents) {
            return;
        }
        let Some(ix) = self.agent_ix(Who::Main) else { return };
        let TabKind::Terminal(view) = &self.tabs[ix].kind else { return };
        let a = &self.agent;
        let resumable = a.launch.as_ref().is_some_and(|l| {
            let adapter = l.profile.adapter();
            l.session_id().is_some() && adapter.resumes() && adapter.events() == crate::agents::Events::All && l.hooks().is_some()
        });
        if !resumable || !matches!(a.status, Idle | Done) || a.timers || a.queued.is_some() || a.pending_prompt.is_some() {
            return;
        }
        let state = view.read(cx).state.read(cx);
        if state.typed_at().is_some_and(|typed| a.heard_at.is_none_or(|heard| typed > heard)) {
            return;
        }
        let Some(pid) = state.foreground().filter(|p| !p.is_shell()).map(|p| p.pid()) else { return };
        self.agent_sleep_probe = Some(std::time::Instant::now());
        cx.spawn(async move |this, cx| {
            let busy = cx.background_spawn(async move { crate::procs::runs_a_shell(pid) }).await;
            let _ = this.update(cx, |t, cx| {
                // A minute before it is looked at again only when it was busy.
                if !busy {
                    t.agent_sleep_probe = None;
                    t.sleep_main_agent(pid, cx);
                }
            });
        })
        .detach();
    }

    /// Its tab keeps its place, saying it is asleep; the agent ends.
    fn sleep_main_agent(&mut self, pid: u32, cx: &mut Context<Self>) {
        use crate::agent::AgentStatus::*;
        let Some(ix) = self.agent_ix(Who::Main) else { return };
        let TabKind::Terminal(view) = &self.tabs[ix].kind else { return };
        let view = view.clone();
        // Looked at again: it may have been given work meanwhile.
        let same = view.read(cx).state.read(cx).foreground().map(|p| p.pid()) == Some(pid);
        if !same || self.visible || !matches!(self.agent.status, Idle | Done) || self.agent.queued.is_some() {
            return;
        }
        let (id, pane, title) = (self.tabs[ix].id, self.tabs[ix].pane, self.tabs[ix].title.clone());
        self.tabs[ix] = TabItem { id, pane, agent: Some(Who::Main), ..TabItem::new(title, TabKind::Asleep) };
        TermView::end(&view, cx);
        self.agent.launch = self.agent.launch.as_ref().map(|l| l.again());
        self.agent.watch = activity::Watch::default();
        self.agent_slept = true;
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// An open file's text when it has edits not on disk: what the agent
    /// reads of it.
    pub fn unsaved_text(&self, path: &std::path::Path, cx: &App) -> Option<String> {
        self.tabs.iter().find_map(|tab| match &tab.kind {
            TabKind::File(file) => {
                let file = file.read(cx);
                (file.path == path && file.dirty).then(|| file.editor.read(cx).text().to_string())
            }
            _ => None,
        })
    }

    pub fn wake_refresh(&self) {
        let _ = self.refresh_tx.try_send(());
    }

    /// A command in a new terminal of the task, shown.
    pub(super) fn run_in_terminal(&mut self, line: String, env: Vec<(String, String)>, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_seq += 1;
        let seq = self.terminal_seq;
        let mut all = self.terminal_env();
        all.extend(env);
        let tab = Self::make_terminal(&self.root, self.hook_socket.as_ref(), Term::Shell { title: format!("shell {seq}"), seq }, Some(line), all, None, window, cx);
        let at = self.tabs.iter().rposition(|t| matches!(t.kind, TabKind::Terminal(_) | TabKind::Chat(_))).map_or(0, |ix| ix + 1);
        self.tabs.insert(at, tab);
        self.activate(at, window, cx);
    }
}

/// Of `files` (relative to `root`), the one written last at or after
/// `since`; a second off, for file systems that keep times that coarse.
pub(super) fn last_written(root: &std::path::Path, files: &[String], since: std::time::SystemTime) -> Option<String> {
    let since = since.checked_sub(Duration::from_secs(1)).unwrap_or(since);
    files
        .iter()
        .filter_map(|file| Some((std::fs::metadata(root.join(file)).ok()?.modified().ok()?, file)))
        .filter(|(written, _)| *written >= since)
        .max_by_key(|(written, _)| *written)
        .map(|(_, file)| file.clone())
}

#[cfg(test)]
mod follow_tests {
    use super::last_written;
    use std::time::{Duration, SystemTime};

    #[test]
    fn the_file_a_script_wrote_last_is_the_one_to_follow() {
        let dir = std::env::temp_dir().join(format!("ideae-follow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let now = SystemTime::now();
        let at = |name: &str, ago: u64| {
            std::fs::write(dir.join(name), name).unwrap();
            std::fs::File::options().write(true).open(dir.join(name)).unwrap().set_modified(now - Duration::from_secs(ago)).unwrap();
        };
        at("old.txt", 600);
        at("src/first.rs", 20);
        at("src/last.rs", 5);
        let files: Vec<String> = ["old.txt", "src/first.rs", "src/last.rs", "gone.txt"].map(String::from).to_vec();
        assert_eq!(last_written(&dir, &files, now - Duration::from_secs(60)).as_deref(), Some("src/last.rs"));
        assert_eq!(last_written(&dir, &files[..2], now - Duration::from_secs(60)).as_deref(), Some("src/first.rs"));
        assert_eq!(last_written(&dir, &files, now - Duration::from_secs(2)), None, "nothing written since: nothing to show");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod who_tests {
    use super::Who;

    #[test]
    fn going_round_in_circles() {
        let calls = |s: &str| s.chars().map(|c| c.to_string()).collect::<Vec<_>>();
        assert!(super::goes_round(&calls("xaaaa")));
        assert!(super::goes_round(&calls("ababab")));
        assert!(!super::goes_round(&calls("aaa")));
        // Tests run between edits that differ, a file read again now and then.
        assert!(!super::goes_round(&calls("tetftg")));
        assert!(!super::goes_round(&calls("abcabc")));
        assert!(!super::goes_round(&calls("aabaab")));
    }

    #[test]
    fn an_agent_by_its_tag() {
        assert_eq!(Who::Main.tag(), None);
        assert_eq!(Who::Question(3).tag().as_deref(), Some("q-3"));
        assert_eq!(Who::of_tag(Some("q-3")), Who::Question(3));
        // Reports of before, and what is not a question's, are the task's own agent's.
        assert_eq!(Who::of_tag(None), Who::Main);
        assert_eq!(Who::of_tag(Some("q-")), Who::Main);
        assert_eq!(Who::of_tag(Some("other")), Who::Main);
    }
}
