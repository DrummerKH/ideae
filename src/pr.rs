//! A task's pull request (GitHub) or merge request (GitLab), as its code
//! host tells of it (crate::integrations): open, draft, merged or closed,
//! and how its checks went. Nothing known, none shown.

use std::path::Path;

use crate::integrations::{IntError, code_host};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checks {
    None,
    Pending,
    Passed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pr {
    pub number: u64,
    pub url: String,
    pub state: PrState,
    pub checks: Checks,
    /// Review threads nobody resolved yet, and reviews asking for changes.
    pub open_feedback: usize,
}

impl Pr {
    /// "PR #67 ✓", "#67 merged" and the like, for the task row.
    pub fn label(&self) -> String {
        match (self.state, self.checks) {
            (PrState::Merged, _) => format!("#{} merged", self.number),
            (PrState::Closed, _) => format!("#{} closed", self.number),
            (PrState::Draft, _) => format!("#{} draft", self.number),
            (PrState::Open, Checks::Passed) => format!("PR #{} ✓", self.number),
            (PrState::Open, Checks::Failed) => format!("PR #{} ✗", self.number),
            (PrState::Open, Checks::Pending) => format!("PR #{} …", self.number),
            (PrState::Open, Checks::None) => format!("PR #{}", self.number),
        }
    }
}

impl PrState {
    pub fn word(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Draft => "draft",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        }
    }

    pub fn from_word(word: &str) -> Option<Self> {
        [PrState::Open, PrState::Draft, PrState::Merged, PrState::Closed].into_iter().find(|s| s.word() == word)
    }
}

/// One review thread: where it is and what was said in it.
#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub path: String,
    pub line: Option<u64>,
    /// The code moved on since; the comment may not apply any more.
    pub outdated: bool,
    pub hunk: String,
    /// (author, text), oldest first.
    pub comments: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Feedback {
    pub threads: Vec<Thread>,
    /// (author, text) of reviews with a body that asked for changes or
    /// commented.
    pub reviews: Vec<(String, String)>,
}

impl Feedback {
    pub fn len(&self) -> usize {
        self.threads.len() + self.reviews.len()
    }

    /// One message for the agent, like a local review's.
    pub fn message(&self, pr: &Pr) -> String {
        let mut out = format!("Review feedback on PR #{} ({}) to address:\n", pr.number, pr.url);
        let mut n = 0;
        for (author, body) in &self.reviews {
            n += 1;
            out.push_str(&format!("\n{n}. Review by {author}:\n{}\n", body.trim()));
        }
        for t in &self.threads {
            n += 1;
            let at = match t.line {
                Some(line) => format!("{}:{line}", t.path),
                None => t.path.clone(),
            };
            let outdated = if t.outdated { " (outdated: the code changed since)" } else { "" };
            out.push_str(&format!("\n{n}. {at}{outdated}\n"));
            // The hunk ends at the commented line; its last lines are context enough.
            let hunk: Vec<&str> = t.hunk.lines().collect();
            let tail = &hunk[hunk.len().saturating_sub(6)..];
            if !tail.is_empty() {
                out.push_str(&format!("```diff\n{}\n```\n", tail.join("\n")));
            }
            for (author, body) in &t.comments {
                out.push_str(&format!("{author}: {}\n", body.trim()));
            }
        }
        out.push_str("\nResolve each point in the code, or tell me why not.\n");
        out
    }
}

/// The most of a failed CI log sent on: its end, where the error is.
const LOG_LINES: usize = 120;
const LOG_BYTES: usize = 12 * 1024;

/// What an agent reads of a CI log: its errors, each once, and its end.
pub fn log_digest(log: &str) -> String {
    crate::digest::within(crate::digest::for_agent(&crate::project_config::plain(log.as_bytes()), LOG_LINES), LOG_BYTES)
}

/// The request of each of `branches`, in their order, asked at once. None
/// when the host did not answer: then nothing is known. Off the UI thread.
pub fn lookup_all(root: &Path, branches: &[String]) -> Option<Vec<Option<Pr>>> {
    code_host(root).and_then(|host| host.prs_for_branches(root, branches)).ok()
}

/// The request whose head is `branch`. Off the UI thread.
pub fn lookup(root: &Path, branch: &str) -> Option<Pr> {
    if branch.is_empty() || branch.starts_with('(') {
        return None;
    }
    lookup_all(root, &[branch.to_string()]).and_then(|mut all| all.pop().flatten())
}

/// A request by its number as it is now, with the branch it is of: an
/// archived task's, which has no branch here any more. Off the UI thread.
pub fn by_number(root: &Path, number: u64) -> Option<(Pr, String)> {
    code_host(root).and_then(|host| host.pr(root, number)).ok()
}

/// Its unresolved review threads and asking reviews. Off the UI thread.
pub fn feedback(root: &Path, pr: &Pr) -> Option<Feedback> {
    code_host(root).and_then(|host| host.feedback(root, pr)).ok()
}

/// The latest failed CI run of `branch`: its address and its log's end.
pub fn failed_log(root: &Path, branch: &str) -> Result<(String, String), String> {
    code_host(root).and_then(|host| host.failed_ci_log(root, branch)).map_err(|e: IntError| e.to_string())
}
