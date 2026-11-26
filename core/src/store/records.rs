//! The records of a machine's work that its store keeps and the window
//! shows: what an agent started, a task's links, its questions, its agents'
//! sessions, its review notes. One definition, the window's and the store's.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn is_zero<T: Default + PartialEq>(n: &T) -> bool {
    *n == T::default()
}

/// One agent a task's agent started (workspace/crew.rs): a question
/// (`main`, `n`), a task (`path`), one proposed (`title`, `prompt`) or declined.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct CrewRecord {
    pub id: u32,
    pub parent: PathBuf,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "is_zero")]
    pub n: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prompt: String,
}

/// A task's link: a ticket, its pull or merge request, or any address
/// (an error's page, a thread). Its data, not a setting: kept with the
/// task and its archive, the integration's id and the address made then.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct TaskLink {
    /// "ticket", "pr" or "url".
    pub what: String,
    /// A ticket's key (`DEV-123`, `group/app#4`, `#4`), a request's `#12`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// The tracker's kind (`jira`), and the integration that owns the key.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tracker: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub integration: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// What reading it with your token found, when it was read: its title
    /// and status; `missing` when the tracker has no such ticket.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

impl TaskLink {
    pub fn is_ticket(&self) -> bool {
        self.what == "ticket"
    }
}

/// A question asked of an agent of its own beside a task's agent.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct QuestionState {
    /// Its number in the task, never taken again: `q-<id>` in its reports.
    pub id: u32,
    /// What it was asked, then the name its agent for quick questions gave it.
    pub title: String,
    /// The profile that answers, and its session, once known.
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Unix seconds.
    pub created: u64,
    /// When it was closed; open while 0.
    #[serde(skip_serializing_if = "is_zero")]
    pub closed: u64,
    /// When its last message was, yours or its agent's answer: to the minute.
    #[serde(skip_serializing_if = "is_zero")]
    pub active: u64,
    /// The files its agent wrote, in the task's folder: what it takes along
    /// when it becomes a task of its own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub wrote: Vec<String>,
    /// Its agent ended with a pause of its task: it starts again when the
    /// question is opened, not with the task.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub asleep: bool,
    /// Its agent has answered and the answer was not looked at.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unread: bool,
    /// Asked by the agent of this task (crew.rs): its answers are that
    /// agent's to read, not yours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_by: Option<PathBuf>,
    /// Its agent's last answer, to read from its row.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub answer: String,
    /// When its tab was last brought up: how recent it is among the tasks.
    #[serde(skip_serializing_if = "is_zero")]
    pub opened: u64,
}

/// A session of an agent in a task.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct AgentSession {
    /// The profile it ran.
    pub profile: String,
    /// The agent's own id for it, once known: what is resumed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Unix seconds.
    pub started: u64,
}

/// A review comment on lines of the diff.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ReviewNote {
    /// "path:12-14".
    pub location: String,
    pub code: String,
    pub language: String,
    pub text: String,
    /// Where it shows in the diff: under this line of the file.
    pub path: String,
    pub modified_side: bool,
    pub line: usize,
    /// Times it went to the agent; 0: not yet. It stays after, until you
    /// are done with it.
    #[serde(skip_serializing_if = "is_zero")]
    pub sent: u32,
    /// After the agent's turn: whether the code it is on is gone or
    /// changed (true), or still there as it was (false), moved to where it
    /// is now. None: not looked at since it was sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
}

#[cfg(test)]
#[test]
fn a_review_note_is_found_where_its_code_went() {
    let note = ReviewNote { code: "let a = 1;\nlet b = 2;\n".into(), line: 8, sent: 1, ..Default::default() };
    // Moved down by lines written above it; the nearer of two copies.
    assert_eq!(note.find_in("x\nlet a = 1;\nlet b = 2;\ny\nz\nw\nq\nlet a = 1;\nlet b = 2;\n"), Some(9));
    assert_eq!(note.find_in("fn f() {}\n"), None, "changed: not there");
    assert!(!note.open() && ReviewNote { changed: Some(false), ..note.clone() }.open());
}

impl ReviewNote {
    /// To go with the next Send: never sent, or sent and its code is still
    /// as it was after the agent's turn.
    pub fn open(&self) -> bool {
        self.sent == 0 || self.changed == Some(false)
    }

    /// Where its code is in `text` (the file now): the place nearest to
    /// where it was, its end line; none when the code is no longer there.
    pub fn find_in(&self, text: &str) -> Option<usize> {
        let code = self.code.trim_end_matches('\n');
        if code.trim().is_empty() {
            return None;
        }
        let span = code.matches('\n').count();
        text.match_indices(code)
            .map(|(at, _)| text.as_bytes().iter().take(at).filter(|&&b| b == b'\n').count() + 1 + span)
            .min_by_key(|end| end.abs_diff(self.line))
    }
}

