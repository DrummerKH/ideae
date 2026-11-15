//! A task's pull request, as `gh` sees it: open, draft, merged or closed, and
//! how its checks went. No `gh`, no login, no GitHub remote: no PR shown.

use std::path::Path;

use serde_json::Value;

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

/// The PR whose head is `branch`, run off the UI thread.
pub fn lookup(root: &Path, branch: &str) -> Option<Pr> {
    if branch.is_empty() || branch.starts_with('(') {
        return None;
    }
    let out = crate::procs::command("gh")
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .args(["pr", "view", branch, "--json", "number,url,state,isDraft,statusCheckRollup"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut pr = parse(&serde_json::from_slice(&out.stdout).ok()?)?;
    if matches!(pr.state, PrState::Open | PrState::Draft) {
        pr.open_feedback = feedback(root, &pr).map_or(0, |f| f.len());
    }
    Some(pr)
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

/// "owner", "repo" from a PR's URL.
fn owner_repo(url: &str) -> Option<(String, String)> {
    let rest = url.split("github.com/").nth(1)?;
    let mut parts = rest.split('/');
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

const FEEDBACK_QUERY: &str = "query($owner:String!,$name:String!,$n:Int!){repository(owner:$owner,name:$name){pullRequest(number:$n){\
reviewThreads(first:100){nodes{isResolved isOutdated path line originalLine comments(first:30){nodes{body author{login} diffHunk}}}}\
reviews(last:50){nodes{state body author{login}}}}}}";

/// The unresolved review threads and asking reviews of `pr`, from GitHub.
pub fn feedback(root: &Path, pr: &Pr) -> Option<Feedback> {
    let (owner, name) = owner_repo(&pr.url)?;
    let out = crate::procs::command("gh")
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .args(["api", "graphql", "-f", &format!("query={FEEDBACK_QUERY}")])
        .args(["-F", &format!("owner={owner}"), "-F", &format!("name={name}"), "-F", &format!("n={}", pr.number)])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_feedback(&serde_json::from_slice(&out.stdout).ok()?)
}

fn parse_feedback(v: &Value) -> Option<Feedback> {
    let pr = &v["data"]["repository"]["pullRequest"];
    let mut feedback = Feedback::default();
    for t in pr["reviewThreads"]["nodes"].as_array()? {
        if t["isResolved"].as_bool() == Some(true) {
            continue;
        }
        let comments = t["comments"]["nodes"].as_array().cloned().unwrap_or_default();
        feedback.threads.push(Thread {
            path: t["path"].as_str().unwrap_or_default().to_string(),
            line: t["line"].as_u64().or(t["originalLine"].as_u64()),
            outdated: t["isOutdated"].as_bool() == Some(true),
            hunk: comments.first().and_then(|c| c["diffHunk"].as_str()).unwrap_or_default().to_string(),
            comments: comments
                .iter()
                .map(|c| (c["author"]["login"].as_str().unwrap_or("someone").to_string(), c["body"].as_str().unwrap_or_default().to_string()))
                .collect(),
        });
    }
    for r in pr["reviews"]["nodes"].as_array().into_iter().flatten() {
        let body = r["body"].as_str().unwrap_or_default().trim();
        if !body.is_empty() && matches!(r["state"].as_str(), Some("CHANGES_REQUESTED" | "COMMENTED")) {
            feedback.reviews.push((r["author"]["login"].as_str().unwrap_or("someone").to_string(), body.to_string()));
        }
    }
    Some(feedback)
}

fn parse(v: &Value) -> Option<Pr> {
    let state = match v["state"].as_str()? {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ if v["isDraft"].as_bool() == Some(true) => PrState::Draft,
        _ => PrState::Open,
    };
    Some(Pr {
        number: v["number"].as_u64()?,
        url: v["url"].as_str()?.to_string(),
        state,
        checks: checks(&v["statusCheckRollup"]),
        open_feedback: 0,
    })
}

/// Check runs report status and conclusion, commit statuses a state.
fn checks(rollup: &Value) -> Checks {
    let Some(items) = rollup.as_array().filter(|a| !a.is_empty()) else { return Checks::None };
    let mut pending = false;
    for item in items {
        let outcome = item["conclusion"].as_str().filter(|s| !s.is_empty()).or(item["state"].as_str()).unwrap_or("");
        match outcome {
            "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => return Checks::Failed,
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            _ => pending = true,
        }
        if item["status"].as_str().is_some_and(|s| s != "COMPLETED") {
            pending = true;
        }
    }
    if pending { Checks::Pending } else { Checks::Passed }
}

#[cfg(test)]
mod tests {
    use super::{Checks, PrState, parse, parse_feedback};

    #[test]
    fn states_and_checks() {
        let pr = |json: &str| parse(&serde_json::from_str(json).unwrap()).unwrap();
        let open = r#"{"number":5,"url":"u","state":"OPEN","isDraft":false,"statusCheckRollup":[
            {"status":"COMPLETED","conclusion":"SUCCESS"},{"status":"IN_PROGRESS","conclusion":""}]}"#;
        assert_eq!((pr(open).state, pr(open).checks), (PrState::Open, Checks::Pending));
        let failed = r#"{"number":5,"url":"u","state":"OPEN","isDraft":true,"statusCheckRollup":[
            {"status":"COMPLETED","conclusion":"FAILURE"},{"state":"SUCCESS"}]}"#;
        assert_eq!((pr(failed).state, pr(failed).checks), (PrState::Draft, Checks::Failed));
        let merged = r#"{"number":67,"url":"u","state":"MERGED","isDraft":false,"statusCheckRollup":[{"state":"SUCCESS"}]}"#;
        assert_eq!((pr(merged).state, pr(merged).checks), (PrState::Merged, Checks::Passed));
        assert_eq!(pr(merged).label(), "#67 merged");
    }

    /// Needs gh and the network: `cargo test live_feedback -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_feedback() {
        let pr = super::Pr { number: 13400, url: "https://github.com/cli/cli/pull/13400".into(), state: PrState::Open, checks: Checks::None, open_feedback: 0 };
        let f = super::feedback(std::path::Path::new("."), &pr).expect("gh answered");
        assert!(f.len() > 0);
        println!("{}", f.message(&pr));
    }

    #[test]
    fn unresolved_threads_and_asking_reviews() {
        let v: serde_json::Value = serde_json::from_str(r#"{"data":{"repository":{"pullRequest":{
            "reviewThreads":{"nodes":[
                {"isResolved":true,"isOutdated":false,"path":"a.rs","line":3,"originalLine":3,"comments":{"nodes":[{"body":"done","author":{"login":"r"},"diffHunk":"@@"}]}},
                {"isResolved":false,"isOutdated":true,"path":"b.rs","line":null,"originalLine":9,"comments":{"nodes":[
                    {"body":"rename this","author":{"login":"rev"},"diffHunk":"@@ -1 +1 @@\n-old\n+new"},
                    {"body":"agreed","author":{"login":"me"},"diffHunk":""}]}}]},
            "reviews":{"nodes":[{"state":"APPROVED","body":"lgtm","author":{"login":"x"}},{"state":"CHANGES_REQUESTED","body":"Needs tests","author":{"login":"rev"}}]}
        }}}}"#).unwrap();
        let f = parse_feedback(&v).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f.threads[0].line, Some(9));
        assert!(f.threads[0].outdated);
        assert_eq!(f.reviews, vec![("rev".to_string(), "Needs tests".to_string())]);
        let pr = super::Pr { number: 5, url: "https://github.com/o/r/pull/5".into(), state: PrState::Open, checks: Checks::None, open_feedback: 2 };
        let message = f.message(&pr);
        assert!(message.contains("b.rs:9 (outdated"), "{message}");
        assert!(message.contains("rev: rename this"));
        assert!(message.contains("Review by rev:\nNeeds tests"));
    }
}
