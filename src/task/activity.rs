//! What an agent that reports little does, guessed from its terminal: it
//! works while it writes (and nobody types), it is done once quiet for a
//! while; a bell or a desktop notification it asks for says it is done or
//! asks something. Guesses are marked as such (`AgentState::guessed`).

use super::*;
use crate::agent::{AgentStatus, HookEvent};

/// Seconds of output, nobody typing, before it counts as at work; of
/// quiet before it counts as done.
const WORKING_AFTER: u32 = 2;
const DONE_AFTER: u32 = 5;

#[derive(Default)]
pub(super) struct Watch {
    output: u64,
    active: u32,
    quiet: u32,
    _task: Option<Task<()>>,
}

/// Of a notification's words: a question waits for the user.
pub(super) fn asks(body: &str) -> bool {
    let body = body.to_lowercase();
    ["approv", "permission", "question", "input", "allow", "confirm", "waiting for you"].iter().any(|w| body.contains(w)) || body.trim_end().ends_with('?')
}

impl TaskView {
    /// For an agent whose adapter does not report when it starts work.
    pub(super) fn watch_activity(&mut self, who: Who, cx: &mut Context<Self>) {
        let Some(agent) = self.agent_of(who) else { return };
        let watched = agent.launch.as_ref().is_some_and(|l| l.events() != crate::agents::Events::All);
        let watch = if watched {
            let task = cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                match this.update(cx, |t, cx| t.activity_tick(who, cx)) {
                    Ok(true) => {}
                    _ => break,
                }
            });
            Watch { _task: Some(task), ..Watch::default() }
        } else {
            Watch::default()
        };
        if let Some(agent) = self.agent_of_mut(who) {
            agent.watch = watch;
        }
    }

    /// False once the agent is gone from the task.
    fn activity_tick(&mut self, who: Who, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.agent_terminal(who, cx) else { return self.agent_of(who).is_some() };
        let state = state.read(cx);
        let output = state.output_seen();
        let running = state.foreground().is_some_and(|p| !p.is_shell());
        let typed = state.typed_at().is_some_and(|at| at.elapsed() < Duration::from_millis(1500));
        let Some(agent) = self.agent_of_mut(who) else { return false };
        let watch = &mut agent.watch;
        let grew = output != watch.output;
        watch.output = output;
        if !running {
            (watch.active, watch.quiet) = (0, 0);
            return true;
        }
        match (grew, typed) {
            (true, false) => (watch.active, watch.quiet) = (watch.active + 1, 0),
            (true, true) => (watch.active, watch.quiet) = (0, 0),
            (false, _) => (watch.active, watch.quiet) = (0, watch.quiet + 1),
        }
        let working = agent.status == AgentStatus::Working;
        if !working && watch.active >= WORKING_AFTER {
            self.guess(who, "UserPromptSubmit", None, cx);
        } else if working && agent.guessed && watch.quiet >= DONE_AFTER {
            self.guess(who, "Stop", None, cx);
        }
        true
    }

    /// A bell or a notification from the agent's terminal.
    pub(super) fn agent_signal(&mut self, who: Who, notification: Option<&str>, cx: &mut Context<Self>) {
        let Some(agent) = self.agent_of(who) else { return };
        if agent.launch.as_ref().is_none_or(|l| l.events() == crate::agents::Events::All) {
            return;
        }
        match notification {
            Some(body) if asks(body) => self.guess(who, "Notification", Some(body.to_string()), cx),
            _ if agent.status == AgentStatus::Working || notification.is_some() => self.guess(who, "Stop", None, cx),
            _ => {}
        }
    }

    fn guess(&mut self, who: Who, event: &str, message: Option<String>, cx: &mut Context<Self>) {
        cx.emit(TaskEvent::Heard(HookEvent {
            task: self.root.clone(),
            agent: who.tag(),
            event: event.into(),
            kind: (event == "Notification").then(|| "guessed".to_string()),
            message,
            file: None,
            said: None,
            replayed: None,
            session: None,
            guessed: true,
            worktree: None,
            call: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn questions_by_their_words() {
        assert!(super::asks("Approval requested: cargo test"));
        assert!(super::asks("Codex wants to edit main.rs?"));
        assert!(!super::asks("Agent turn complete"));
    }
}
