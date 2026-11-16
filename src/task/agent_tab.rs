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
    pub(super) fn agent_reports(&self) -> bool {
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
}
