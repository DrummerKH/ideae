use super::*;

impl TaskView {
    /// Puts code into the agent's prompt, without sending it: the location,
    /// then the code fenced, so you can type the question after it.
    pub fn send_to_agent(&mut self, location: String, code: String, language: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = format!("{location}\n```{language}\n{}\n```\n", code.trim_end_matches('\n'));
        self.paste_to_agent(text, window, cx);
    }

    /// Starts the agent again in its tab: typed into its shell, or in a new
    /// terminal when the shell is gone too.
    pub fn restart_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::Chat(_))) {
            if let TabKind::Chat(chat) = &self.tabs[ix].kind {
                chat.clone().update(cx, |c, cx| c.restart(cx));
            }
            self.chat_told = None;
            self.agent_status = crate::agent::AgentStatus::Unknown;
            self.activate(ix, window, cx);
            cx.emit(TaskEvent::Changed);
            return cx.notify();
        }
        let Some(launch) = self.launch.as_ref().map(|l| l.again()) else { return };
        let command = launch.command();
        let Some(ix) = self.tabs.iter().position(|t| t.title == "agent" && matches!(t.kind, TabKind::Terminal(_))) else {
            return;
        };
        let TabKind::Terminal(view) = &self.tabs[ix].kind else { return };
        let view = view.clone();
        let state = view.read(cx).state.clone();
        if TermView::gone(state.read(cx)) {
            TermView::end(&view, cx);
            let mut env = self.terminal_env();
            env.extend(launch.env());
            let tab = Self::make_terminal(&self.root, self.hook_socket.as_ref(), "agent", 0, Some(command), env, None, None, window, cx);
            (self.tabs[ix].kind, self.tabs[ix].shown) = (tab.kind, tab.shown);
        } else {
            let enter = if cfg!(windows) { "\r" } else { "\n" };
            view.update(cx, |v, _| v.ran = false);
            state.update(cx, |s, cx| s.send_text(format!("{command}{enter}"), cx));
        }
        self.launch = Some(launch);
        self.agent_status = crate::agent::AgentStatus::Unknown;
        self.status_guessed = false;
        self.activate(ix, window, cx);
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// Another agent in the agent's tab: the one running ends with its
    /// terminal, the new one starts in a terminal of its own, with its
    /// own environment.
    pub fn switch_agent(&mut self, launch: crate::agents::Launch, window: &mut Window, cx: &mut Context<Self>) {
        let task_env = self.terminal_env();
        // The conversation before was another agent's: not the new one's to go on with.
        if let Some(chat) = self.agent_chat() {
            chat.update(cx, |c, _| c.end());
        }
        // A new conversation: the old one's journal, log and checkpoints go.
        // The files now, before the new host opens them (an unlink each);
        // the refs, which take git, off this thread.
        let journal = crate::acp::journal_path(&self.root);
        let log = crate::acp::host::log_path(&journal);
        for old in [journal.clone(), log.with_extension("log.old"), log] {
            let _ = std::fs::remove_file(old);
        }
        let root = self.root.clone();
        std::thread::spawn(move || crate::acp::checkpoint::forget(&root, &crate::acp::checkpoint::thread_name(Some(&journal))));
        let tab = if launch.profile.acp {
            Self::make_chat(&self.root, &launch, task_env, window, cx)
        } else {
            let mut env = task_env;
            env.extend(launch.env());
            Self::make_terminal(&self.root, self.hook_socket.as_ref(), "agent", 0, Some(launch.command()), env, None, None, window, cx)
        };
        self.chat_told = None;
        let ix = match self.tabs.iter().position(|t| t.title == "agent" && matches!(t.kind, TabKind::Terminal(_) | TabKind::Chat(_))) {
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
        self.launch = Some(launch);
        self.agent_status = crate::agent::AgentStatus::Unknown;
        self.status_guessed = false;
        self.agent_said = None;
        self.agent_asks = None;
        self.queued_message = None;
        self.watch_activity(cx);
        self.activate(ix, window, cx);
        cx.emit(TaskEvent::Changed);
        cx.notify();
    }

    /// The agent said which session it runs: a restart resumes that one.
    pub fn learned_session(&mut self, id: &str) {
        if let Some(launch) = &mut self.launch {
            launch.learned(id);
        }
    }

    /// The session the agent runs, to find its file by.
    pub fn session_ref(&self) -> Option<crate::agents::SessionRef> {
        let launch = self.launch.as_ref()?;
        Some(crate::agents::SessionRef { profile: launch.profile.clone(), id: launch.session_id() })
    }

    /// The agent's mark, when there are several to tell apart.
    pub fn agent_letter(&self) -> Option<String> {
        let launch = self.launch.as_ref()?;
        crate::state::with_settings(|s| s.several_agents()).then(|| launch.profile.letter())
    }

    /// Puts `text` into the agent's prompt once the agent is up: Claude Code
    /// says so with SessionStart; another agent gets a few seconds after it
    /// starts. Pasted before, the shell would read it.
    ///
    /// With `send` it is sent as well, not left for the user to finish.
    pub fn queue_prompt(&mut self, text: String, send: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.launch.is_none() {
            return;
        }
        // Its host takes a prompt whenever it comes, and sends it when the
        // agent is up.
        if let Some(chat) = self.agent_chat() {
            chat.update(cx, |c, cx| c.put(text, send, window, cx));
            return;
        }
        self.pending_prompt = Some((text, send));
        cx.spawn_in(window, async move |this, cx| {
            let started = std::time::Instant::now();
            let mut running_since: Option<std::time::Instant> = None;
            // When its output last grew: an agent that tells nothing is
            // up once it has drawn its screen and gone quiet.
            let (mut output, mut wrote_at) = (0u64, std::time::Instant::now());
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let ready = this.update(cx, |t, cx| {
                    if t.pending_prompt.is_none() {
                        return None;
                    }
                    let hooked = !matches!(t.agent_status, crate::agent::AgentStatus::Unknown);
                    let state = t.agent_terminal(cx)?;
                    let state = state.read(cx);
                    let running = state.foreground().is_some_and(|p| !p.is_shell());
                    Some((hooked, running, state.output_seen()))
                });
                let Ok(Some((hooked, running, seen))) = ready else { return };
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
                let patient = this.read_with(cx, |t, _| t.agent_reports()).unwrap_or(false);
                if (hooked && waited > Duration::from_millis(500))
                    || (!patient && (settled || waited > Duration::from_secs(8) || started.elapsed() > Duration::from_secs(30)))
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
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::Chat(_))) {
            if let TabKind::Chat(chat) = &self.tabs[ix].kind {
                chat.clone().update(cx, |c, cx| c.put(text, false, window, cx));
            }
            return self.activate(ix, window, cx);
        }
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
        if let Some(chat) = self.agent_chat() {
            chat.update(cx, |c, cx| c.put(text, true, window, cx));
            return;
        }
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
    pub(super) fn agent_reports(&self) -> bool {
        self.launch.as_ref().is_some_and(|l| l.profile.adapter().reports_ready() && crate::agents::hooks().is_some())
    }

    /// The agent tab's terminal.
    pub(super) fn agent_terminal(&self, cx: &App) -> Option<Entity<TerminalState>> {
        self.tabs.iter().find_map(|tab| match &tab.kind {
            TabKind::Terminal(view) if tab.title == "agent" => Some(view.read(cx).state.clone()),
            _ => None,
        })
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
}

// ---------------------------------------------------------------- chat agents

impl TaskView {
    /// The tab of an agent that speaks ACP: its host is the task's, in the
    /// daemon when there is one.
    pub(super) fn make_chat(root: &PathBuf, launch: &crate::agents::Launch, task_env: Vec<(String, String)>, window: &mut Window, cx: &mut Context<Self>) -> TabItem {
        let spec = Self::chat_spec(root, launch, task_env);
        let id = format!("{}#acp", root.display());
        let task = cx.weak_entity();
        let chat = cx.new(|cx| ChatView::new(id, spec, task, window, cx));
        let profile = launch.profile.id.clone();
        cx.subscribe_in(&chat, window, move |this: &mut Self, chat, event: &ChatEvent, window, cx| match event {
            ChatEvent::Follow { path, line } => this.follow_file(chat.clone(), path.clone(), *line, window, cx),
            ChatEvent::Changed => this.chat_changed(chat, cx),
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
        TabItem::new("agent", TabKind::Chat(chat))
    }

    fn chat_spec(root: &PathBuf, launch: &crate::agents::Launch, mut env: Vec<(String, String)>) -> crate::acp::wire::HostSpec {
        use crate::acp::policy::{CHOICE, Policy};
        env.extend(launch.profile.env_vars());
        env.push((crate::agent::ROLE_ENV.into(), "agent".into()));
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
            journal: Some(crate::acp::journal_path(root)),
            unasked: launch.unasked,
            // What was chosen last for this agent, then what this task asks.
            options: launch.profile.choices.iter().filter(|(k, _)| k.as_str() != CHOICE).map(|(k, v)| (k.clone(), v.clone())).chain(launch.options.iter().cloned()).collect::<BTreeMap<_, _>>().into_iter().collect(),
            policy: policy.id().to_string(),
            mcp: crate::mcp::server_for(root),
            unanswered_secs: crate::state::with_settings(|s| u64::from(s.agent_unanswered_minutes) * 60),
            notify_as: crate::state::with_settings(|s| s.notifications).then(|| launch.profile.name.clone()),
            budget: launch.profile.daily_limit.map(|limit| (limit - crate::acp::spend::spent_today(&launch.profile.id)).max(0.0)),
        }
    }

    /// A chat agent asked to go on with a session: if it cannot, this is
    /// its first prompt.
    pub fn otherwise_tell(&self, text: String, cx: &mut Context<Self>) {
        if let Some(chat) = self.agent_chat() {
            chat.read(cx).send(crate::acp::wire::ToHost::Otherwise { prompt: text });
        }
    }

    /// The chat agent's tab, when the task runs one.
    pub(super) fn agent_chat(&self) -> Option<Entity<ChatView>> {
        self.tabs.iter().find_map(|tab| match &tab.kind {
            TabKind::Chat(chat) => Some(chat.clone()),
            _ => None,
        })
    }

    /// The chat agent's status, told as the hooks of a terminal agent are:
    /// the workspace notifies, marks the task and keeps the session.
    fn chat_changed(&mut self, chat: &Entity<ChatView>, cx: &mut Context<Self>) {
        use crate::acp::thread::Status;
        let thread = &chat.read(cx).thread;
        if let Some(usage) = &thread.usage {
            if usage.size > 0 {
                self.context_fill = (usage.used as f32 / usage.size as f32).min(1.);
            }
            // What it cost, counted for its profile's day.
            if let (Some(cost), Some(session), Some(launch)) = (usage.cost.as_deref().and_then(crate::acp::spend::usd), &thread.session, &self.launch) {
                crate::acp::spend::record(&launch.profile.id, session, cost);
            }
        }
        let session = thread.session.clone();
        if let Some(id) = &session {
            self.learned_session(id);
        }
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
        if self.chat_told.as_ref() == Some(&told) {
            return;
        }
        self.chat_told = Some(told);
        let said = thread.last_said().map(|s| s.to_string());
        cx.emit(TaskEvent::Heard(crate::agent::HookEvent {
            task: self.root.clone(),
            event: event.into(),
            message,
            kind,
            file: None,
            said: said.filter(|_| event == "Stop" || event == "Notification"),
            replayed: None,
            session,
            guessed: false,
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
            self.beside_chat(ix, window, cx);
        }
    }

    /// Tab `ix` in the other column than the chat's (made if need be), the
    /// chat in front in its own pane, keeping the keyboard.
    pub(super) fn beside_chat(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat_pane) = self.tabs.iter().find(|t| matches!(t.kind, TabKind::Chat(_))).map(|t| t.pane) else { return };
        if self.tabs.get(ix).is_some_and(|t| t.pane.right == chat_pane.right) {
            self.move_to(ix, Pane { right: !chat_pane.right, below: false }, window, cx);
        }
        if let Some(chat_ix) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::Chat(_))) {
            self.activate(chat_ix, window, cx);
        }
    }

    /// A chat agent with nothing to do stops; its conversation stays.
    pub fn put_agent_to_sleep(&mut self, cx: &mut Context<Self>) {
        if let Some(chat) = self.agent_chat() {
            chat.update(cx, |c, cx| c.sleep(cx));
        }
    }

    pub fn wake_agent(&mut self, cx: &mut Context<Self>) {
        if let Some(chat) = self.agent_chat() {
            chat.update(cx, |c, cx| c.wake(cx));
        }
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
        let tab = Self::make_terminal(&self.root, self.hook_socket.as_ref(), &format!("shell {seq}"), seq, Some(line), all, None, None, window, cx);
        let at = self.tabs.iter().rposition(|t| matches!(t.kind, TabKind::Terminal(_) | TabKind::Chat(_))).map_or(0, |ix| ix + 1);
        self.tabs.insert(at, tab);
        self.activate(at, window, cx);
    }
}
