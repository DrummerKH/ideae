//! What an agent that reports little does, guessed from its terminal: it
//! works while it writes (and nobody types), it is done once quiet for a
//! while; a bell or a desktop notification it asks for says it is done or
//! asks something. Guesses are marked as such (`status_guessed`).

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
    pub(super) fn watch_activity(&mut self, cx: &mut Context<Self>) {
        let watched = self.launch.as_ref().is_some_and(|l| l.profile.adapter().events() != crate::agents::Events::All);
        if !watched {
            self.activity = Watch::default();
            return;
        }
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if this.update(cx, |t, cx| t.activity_tick(cx)).is_err() {
                break;
            }
        });
        self.activity = Watch { _task: Some(task), ..Watch::default() };
    }

    fn activity_tick(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.agent_terminal(cx) else { return };
        let state = state.read(cx);
        let output = state.output_seen();
        let running = state.foreground().is_some_and(|p| !p.is_shell());
        let typed = state.typed_at().is_some_and(|at| at.elapsed() < Duration::from_millis(1500));
        let grew = output != self.activity.output;
        self.activity.output = output;
        if !running {
            (self.activity.active, self.activity.quiet) = (0, 0);
            return;
        }
        match (grew, typed) {
            (true, false) => (self.activity.active, self.activity.quiet) = (self.activity.active + 1, 0),
            (true, true) => (self.activity.active, self.activity.quiet) = (0, 0),
            (false, _) => (self.activity.active, self.activity.quiet) = (0, self.activity.quiet + 1),
        }
        let working = self.agent_status == AgentStatus::Working;
        if !working && self.activity.active >= WORKING_AFTER {
            self.guess("UserPromptSubmit", None, cx);
        } else if working && self.status_guessed && self.activity.quiet >= DONE_AFTER {
            self.guess("Stop", None, cx);
        }
    }

    /// A bell or a notification from the agent's terminal.
    pub(super) fn agent_signal(&mut self, notification: Option<&str>, cx: &mut Context<Self>) {
        if self.launch.as_ref().is_none_or(|l| l.profile.adapter().events() == crate::agents::Events::All) {
            return;
        }
        match notification {
            Some(body) if asks(body) => self.guess("Notification", Some(body.to_string()), cx),
            _ if self.agent_status == AgentStatus::Working || notification.is_some() => self.guess("Stop", None, cx),
            _ => {}
        }
    }

    fn guess(&mut self, event: &str, message: Option<String>, cx: &mut Context<Self>) {
        cx.emit(TaskEvent::Heard(HookEvent {
            task: self.root.clone(),
            event: event.into(),
            kind: (event == "Notification").then(|| "guessed".to_string()),
            message,
            file: None,
            said: None,
            replayed: None,
            session: None,
            guessed: true,
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
