//! Between the window and the host of an agent: one JSON message a line, in
//! both directions. Nothing in it assumes the two share a machine: paths are
//! the agent's, files travel as text, so a host can run where the project
//! is and the window elsewhere.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::thread::{Op, Thread};

/// Bumped when the messages change: a window meeting a host of another
/// version starts a new one, which takes the session over.
pub const VERSION: u32 = 1;

/// What the host is to run.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct HostSpec {
    /// The agent's command, as typed in a shell: `opencode acp`.
    pub command: String,
    pub cwd: PathBuf,
    /// For the agent alone, on top of what the host has.
    pub env: Vec<(String, String)>,
    /// The session to go on with, when the agent can.
    pub resume: Option<String>,
    /// Where the thread is kept: it comes back from there on the next start.
    pub journal: Option<PathBuf>,
    /// Questions are answered yes without asking (a variant left to work).
    pub unasked: bool,
    /// Chosen before: set again on a new session (model, mode, effort).
    pub options: Vec<(String, String)>,
    /// Who answers permission questions ([`super::policy::Policy`]'s id).
    pub policy: String,
    /// ideae's MCP server for the agent, scoped to its worktree: the
    /// program, its arguments and variables.
    pub mcp: Option<(String, Vec<String>, Vec<(String, String)>)>,
    /// A question left unanswered this many seconds is declined; 0: it waits.
    pub unanswered_secs: u64,
    /// The agent's name, for a desktop notification while no window is
    /// there to ask in; None: none (tests, notifications off).
    pub notify_as: Option<String>,
    /// What this run may still cost (USD), its profile's daily limit less
    /// what it has cost today; None: no limit.
    pub budget: Option<f64>,
    /// Started by the login shell (`$SHELL -lc`), for its PATH: an agent
    /// on another machine, where no window's environment is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub login_shell: bool,
    /// Where the thread was kept under the name of an earlier build: moved
    /// to `journal` when that has none ([`super::carry_journal`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_before: Option<PathBuf>,
}

/// Window to host.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "m")]
pub enum ToHost {
    /// A window is here: the host answers with the whole thread.
    Attach { version: u32 },
    /// What you wrote, the files it mentions (`@src/a.rs`, sent as links to
    /// them) and the images pasted with it.
    Prompt {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
    },
    Cancel,
    /// An answer to a permission question; None declines it.
    Answer { request: u64, option: Option<String> },
    Choose { id: String, value: String },
    /// Sign in by a method the agent runs itself.
    Authenticate { method: String },
    /// The file as open in the editor, unsaved edits and all; None: not
    /// open, or as on disk.
    Buffer { request: u64, text: Option<String> },
    /// Drop prompts queued while the agent worked.
    Unqueue,
    /// The daemon: the last window went (a question then notifies).
    Detached,
    /// Take back one queued prompt (to change it): dropped from the queue.
    Take { at: usize },
    /// Sent as the first prompt if the session asked for (`resume`) cannot
    /// be gone on with: what a new one needs to know.
    Otherwise { prompt: String },
    /// What putting the files back as at entry `ix`'s checkpoint would change.
    Preview { ix: usize },
    /// Put them back.
    Restore { ix: usize },
    /// Who answers permission questions from now on.
    Policy { policy: String },
    /// End the agent and the host.
    Stop,
}

/// A picture sent with a prompt.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Image {
    pub mime: String,
    /// Base64.
    pub data: String,
}

impl ToHost {
    pub fn prompt(text: impl Into<String>) -> ToHost {
        ToHost::Prompt { text: text.into(), files: Vec::new(), images: Vec::new() }
    }
}

/// Host to window.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "m")]
pub enum ToWindow {
    Hello { version: u32, thread: Box<Thread> },
    Op { op: Op },
    /// The agent reads a file: the window says whether it has it open with
    /// edits not on disk.
    Read { request: u64, path: PathBuf },
    /// The agent wrote a file, through the host.
    Wrote { path: PathBuf },
    /// What restoring entry `ix`'s checkpoint would change: 'A' a file
    /// made since (removed), 'M' changed, 'D' deleted since (back); None:
    /// the checkpoint is gone.
    Restorable { ix: usize, files: Option<Vec<(char, String)>> },
}

/// One message a line. Lines that are not ours (a test harness's own
/// output, a stray print) are skipped by the reader.
pub fn line<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut out = serde_json::to_vec(msg).unwrap_or_default();
    out.push(b'\n');
    out
}

/// Splits bytes into lines as they come.
#[derive(Default)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn feed<'a>(&'a mut self, bytes: &[u8]) -> impl Iterator<Item = Vec<u8>> + 'a {
        self.partial.extend_from_slice(bytes);
        std::iter::from_fn(move || {
            let at = self.partial.iter().position(|&b| b == b'\n')?;
            let mut line: Vec<u8> = self.partial.drain(..=at).collect();
            line.pop();
            Some(line)
        })
    }
}

pub fn parse<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Option<T> {
    let line = line.trim_ascii();
    if !line.starts_with(b"{") {
        return None;
    }
    serde_json::from_slice(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_come_whole_whatever_the_chunks() {
        let mut lines = Lines::default();
        let bytes = [line(&ToHost::Cancel), b"running 1 test\n".to_vec(), line(&ToHost::prompt("a\nb"))].concat();
        let mut got: Vec<ToHost> = Vec::new();
        for chunk in bytes.chunks(7) {
            got.extend(lines.feed(chunk).filter_map(|l| parse(&l)));
        }
        assert_eq!(got, vec![ToHost::Cancel, ToHost::prompt("a\nb")]);
    }
}
