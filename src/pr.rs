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

/// A PR by its number as it is now, with the branch it is of: an archived
/// task's, which has no branch here any more. Off the UI thread.
pub fn by_number(root: &Path, number: u64) -> Option<(Pr, String)> {
    let out = crate::procs::command("gh")
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .args(["pr", "view", &number.to_string(), "--json", "number,url,state,isDraft,headRefName"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some((parse(&v)?, v["headRefName"].as_str().unwrap_or_default().to_string()))
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

/// What `lookup` tells of a PR, asked for many branches at once.
const PR_FIELDS: &str = "nodes{number url state isDraft \
commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){nodes{__typename ... on CheckRun{status conclusion} ... on StatusContext{state}}}}}}} \
reviewThreads(first:100){nodes{isResolved}} reviews(last:50){nodes{state body}}}";

/// The most branches one query asks about.
const BRANCHES_A_QUERY: usize = 20;

/// The PR of each of `branches`, in their order, with one question to
/// GitHub for all of them where `lookup` is two a branch. None when gh
/// did not answer (no gh, no login, no GitHub remote): then nothing is known.
pub fn lookup_all(root: &Path, branches: &[String]) -> Option<Vec<Option<Pr>>> {
    let mut found = Vec::with_capacity(branches.len());
    for chunk in branches.chunks(BRANCHES_A_QUERY) {
        let asked: String = chunk
            .iter()
            .enumerate()
            // A name no branch has asks about none, and keeps the place.
            .map(|(i, branch)| {
                let name = if branch.is_empty() || branch.starts_with('(') { "\"\"".to_string() } else { serde_json::to_string(branch).unwrap_or_default() };
                format!("b{i}:pullRequests(headRefName:{name},first:1,orderBy:{{field:CREATED_AT,direction:DESC}}){{{PR_FIELDS}}} ")
            })
            .collect();
        let query = format!("query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{{asked}}}}}");
        let out = crate::procs::command("gh")
            .current_dir(root)
            .env("GH_PROMPT_DISABLED", "1")
            // gh puts the repository of this directory in their place.
            .args(["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-f", &format!("query={query}")])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let answer: Value = serde_json::from_slice(&out.stdout).ok()?;
        let repository = answer["data"]["repository"].as_object()?;
        found.extend((0..chunk.len()).map(|i| parse_listed(&repository.get(&format!("b{i}"))?["nodes"][0])));
    }
    Some(found)
}

/// A PR as `lookup_all` is told of it.
fn parse_listed(v: &Value) -> Option<Pr> {
    let mut pr = parse(v)?;
    pr.checks = checks(&v["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"]);
    if matches!(pr.state, PrState::Open | PrState::Draft) {
        let open = |thread: &&Value| thread["isResolved"].as_bool() != Some(true);
        let asks = |review: &&Value| !review["body"].as_str().unwrap_or_default().trim().is_empty() && matches!(review["state"].as_str(), Some("CHANGES_REQUESTED" | "COMMENTED"));
        pr.open_feedback = v["reviewThreads"]["nodes"].as_array().into_iter().flatten().filter(open).count() + v["reviews"]["nodes"].as_array().into_iter().flatten().filter(asks).count();
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

/// The most of a failed CI log sent on: its end, where the error is.
const LOG_LINES: usize = 120;
const LOG_BYTES: usize = 12 * 1024;

/// The latest failed run of `branch`'s workflows: its address, and the end
/// of the log of the steps that failed. Run off the UI thread.
pub fn failed_log(root: &Path, branch: &str) -> Result<(String, String), String> {
    let gh = |args: &[&str]| {
        let out = crate::procs::command("gh").current_dir(root).env("GH_PROMPT_DISABLED", "1").args(args).output().map_err(|e| format!("gh: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    };
    let runs: Value = serde_json::from_str(&gh(&["run", "list", "--branch", branch, "--limit", "20", "--json", "databaseId,conclusion,url"])?).map_err(|e| e.to_string())?;
    let failed = runs
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| matches!(r["conclusion"].as_str(), Some("failure" | "timed_out" | "startup_failure")))
        .ok_or_else(|| format!("No failed run of {branch} found"))?;
    let id = failed["databaseId"].as_u64().ok_or("A run without an id")?.to_string();
    let url = failed["url"].as_str().unwrap_or_default().to_string();
    let log = gh(&["run", "view", &id, "--log-failed"])?;
    Ok((url, log_tail(&log)))
}

/// `gh run view --log-failed` gives "job<TAB>step<TAB>time line": the line
/// is what counts, the last of them, within bounds.
fn log_tail(log: &str) -> String {
    let lines: Vec<String> = log
        .lines()
        .map(|line| {
            let text = line.splitn(3, '\t').nth(2).unwrap_or(line);
            // The timestamp: 2024-05-01T10:00:00.1234567Z
            match text.split_once(' ') {
                Some((time, rest)) if time.len() > 20 && time.ends_with('Z') && time.as_bytes()[4] == b'-' => rest.to_string(),
                _ => text.to_string(),
            }
        })
        .collect();
    let mut out = crate::project_config::plain(lines[lines.len().saturating_sub(LOG_LINES)..].join("\n").as_bytes());
    if out.len() > LOG_BYTES {
        let mut cut = out.len() - LOG_BYTES;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out = out[cut..].to_string();
    }
    out
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

    #[test]
    fn a_pr_asked_for_among_many() {
        let v: serde_json::Value = serde_json::from_str(r#"{"number":7,"url":"https://github.com/o/r/pull/7","state":"OPEN","isDraft":false,
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
                {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},{"__typename":"StatusContext","state":"PENDING"}]}}}}]},
            "reviewThreads":{"nodes":[{"isResolved":true},{"isResolved":false}]},
            "reviews":{"nodes":[{"state":"APPROVED","body":"lgtm"},{"state":"COMMENTED","body":" "},{"state":"CHANGES_REQUESTED","body":"tests"}]}}"#).unwrap();
        let pr = super::parse_listed(&v).unwrap();
        assert_eq!((pr.number, pr.state, pr.checks, pr.open_feedback), (7, PrState::Open, Checks::Pending, 2));
        // No checks at all, and no PR of the branch.
        let bare: serde_json::Value = serde_json::from_str(r#"{"number":8,"url":"u","state":"MERGED","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}"#).unwrap();
        assert_eq!(super::parse_listed(&bare).map(|pr| (pr.state, pr.checks)), Some((PrState::Merged, Checks::None)));
        assert!(super::parse_listed(&serde_json::Value::Null).is_none());
    }

    /// Needs gh, a login and the network, in a clone of a GitHub repository
    /// (`IDEAE_PR_ROOT`, of its branches `IDEAE_PR_BRANCHES`, comma-separated):
    /// `cargo test live_lookup_all -- --ignored --nocapture`. Each PR it
    /// finds is told as `lookup` tells it.
    #[test]
    #[ignore]
    fn live_lookup_all() {
        let root = std::env::var("IDEAE_PR_ROOT").unwrap_or_else(|_| ".".into());
        let root = std::path::Path::new(&root);
        let branches: Vec<String> = std::env::var("IDEAE_PR_BRANCHES").unwrap_or_else(|_| "main".into()).split(',').map(str::to_string).collect();
        let all = super::lookup_all(root, &branches).expect("gh answered");
        assert_eq!(all.len(), branches.len());
        for (branch, pr) in branches.iter().zip(&all) {
            println!("{branch}: {pr:?}");
            assert_eq!(*pr, super::lookup(root, branch), "{branch}");
        }
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

    #[test]
    fn a_failed_log_keeps_its_lines_and_their_end() {
        let log = (0..300).map(|i| format!("build\tRun tests\t2024-05-01T10:00:00.1234567Z line {i}")).collect::<Vec<_>>().join("\n");
        let tail = super::log_tail(&log);
        assert!(tail.starts_with("line 180\n") && tail.ends_with("line 299"), "{tail}");
    }
}
