//! What an agent's reports say of it: working, done, waiting for the user.

use std::path::PathBuf;

/// The `kind` of a PreToolUse whose tool waits for the user's answer.
pub const ASKS_USER: &str = "asks_user";
#[derive(Clone, Debug, PartialEq)]
pub enum AgentStatus {
    /// No hook seen yet: the agent is starting or is not Claude.
    Unknown,
    /// Session open, waiting for a first prompt.
    Idle,
    Working,
    /// A permission prompt or a question: the user has to act.
    NeedsInput(String),
    /// Finished its turn.
    Done,
    Exited,
}

impl AgentStatus {
    /// Wants the user's attention.
    pub fn is_attention(&self) -> bool {
        matches!(self, AgentStatus::NeedsInput(_) | AgentStatus::Done)
    }
}

#[derive(Clone, Debug)]
pub struct HookEvent {
    /// The task's worktree root.
    pub task: PathBuf,
    /// Which of its agents, when not its own (`q-<n>`, `IDEAE_AGENT`).
    pub agent: Option<String>,
    pub event: String,
    pub message: Option<String>,
    /// Of a Notification: permission_prompt, idle_prompt, …
    pub kind: Option<String>,
    /// The file a tool call is about to change.
    pub file: Option<String>,
    /// What the agent said : a turn's last words, or what
    /// a tool call waits for.
    pub said: Option<String>,
    /// The daemon's record of an earlier event, handed over on connect, with
    /// when it heard it (0: not known): it sets the status but notifies
    /// nobody again.
    pub replayed: Option<u64>,
    /// The agent's id of the session it runs, when it says.
    pub session: Option<String>,
    /// Not reported but guessed from the agent's terminal.
    pub guessed: bool,
    /// A worktree the agent is about to make with `git worktree add`.
    pub worktree: Option<String>,
    /// Of a tool call: the tool's name, a colon, and a print of the call with
    /// its input, the same for the same call made again.
    pub call: Option<String>,
}

impl HookEvent {
    /// The tool a call is of, as Claude Code names it (`Bash`, `CronCreate`).
    pub fn tool(&self) -> Option<&str> {
        self.call.as_deref().and_then(|c| c.split_once(':')).map(|(tool, _)| tool)
    }
}

impl HookEvent {
    fn is_idle_reminder(&self) -> bool {
        match self.kind.as_deref() {
            Some(kind) => kind == "idle_prompt",
            // Versions without notification_type.
            None => self.message.as_deref().is_some_and(|m| m.contains("waiting for your input")),
        }
    }

    pub fn status(&self) -> Option<AgentStatus> {
        Some(match self.event.as_str() {
            "SessionStart" => AgentStatus::Idle,
            "PreToolUse" if self.kind.as_deref() == Some(ASKS_USER) => AgentStatus::NeedsInput(
                self.message.clone().unwrap_or_else(|| "Asks you a question".into()),
            ),
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => AgentStatus::Working,
            // A minute after a turn ends Claude reminds that it waits; that is
            // no question, the turn is just done.
            "Notification" if self.is_idle_reminder() => AgentStatus::Done,
            "Notification" if self.kind.as_deref() == Some("auth_success") => return None,
            "Notification" => AgentStatus::NeedsInput(
                self.message.clone().unwrap_or_else(|| "The agent needs your input".into()),
            ),
            "Stop" => AgentStatus::Done,
            "SessionEnd" => AgentStatus::Exited,
            _ => return None,
        })
    }
}

/// A Notification about what already waits for the user: the question
/// its tool call (AskUserQuestion) told of before.
pub fn asked_again(was: &AgentStatus, event: &HookEvent) -> bool {
    event.event == "Notification" && matches!(was, AgentStatus::NeedsInput(_)) && matches!(event.status(), Some(AgentStatus::NeedsInput(_)))
}
