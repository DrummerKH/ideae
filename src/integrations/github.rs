//! GitHub as a code host and its issues, through `gh`: run here, where the
//! user signed in. No `gh`, no login, no GitHub remote: nothing known.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{CodeHost, IntError, ListedPr};
use crate::pr::{Checks, Feedback, Pr, PrState, Thread};

pub struct GitHub;

fn gh_run(root: &Path, args: &[&str]) -> Result<Vec<u8>, IntError> {
    let out = gh_for(root)
        .ok_or_else(|| IntError::NoRepository("gh: no GitHub repository known for its origin".into()))?
        .env("GH_PROMPT_DISABLED", "1")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { IntError::ToolMissing("gh") } else { IntError::Network(format!("gh: {e}")) })?;
    if out.status.success() { Ok(out.stdout) } else { Err(IntError::from_cli("gh", "gh auth login", &String::from_utf8_lossy(&out.stderr))) }
}

fn json(bytes: &[u8]) -> Result<Value, IntError> {
    serde_json::from_slice(bytes).map_err(|e| IntError::Parse(e.to_string()))
}

impl CodeHost for GitHub {
    fn prs_for_branches(&self, root: &Path, branches: &[String]) -> Result<Vec<Option<Pr>>, IntError> {
        lookup_all(root, branches)
    }

    fn pr(&self, root: &Path, number: u64) -> Result<(Pr, String), IntError> {
        let v = json(&gh_run(root, &["pr", "view", &number.to_string(), "--json", "number,url,state,isDraft,headRefName"])?)?;
        Ok((parse(&v).ok_or_else(|| IntError::Parse("a pull request without its number".into()))?, v["headRefName"].as_str().unwrap_or_default().to_string()))
    }

    fn request_info(&self, root: &Path, number: u64) -> Result<super::RequestInfo, IntError> {
        let v = json(&gh_run(root, &["pr", "view", &number.to_string(), "--json", "number,url,state,isDraft,headRefName,title,isCrossRepository"])?)?;
        Ok(super::RequestInfo {
            pr: parse(&v).ok_or_else(|| IntError::Parse("a pull request without its number".into()))?,
            head: v["headRefName"].as_str().unwrap_or_default().to_string(),
            title: v["title"].as_str().unwrap_or_default().to_string(),
            cross: v["isCrossRepository"].as_bool().unwrap_or(false),
        })
    }

    fn feedback(&self, root: &Path, pr: &Pr) -> Result<Feedback, IntError> {
        let (owner, name) = owner_repo(&pr.url).ok_or_else(|| IntError::Parse(format!("not a GitHub pull request: {}", pr.url)))?;
        let out = gh_run(
            root,
            &["api", "graphql", "-f", &format!("query={FEEDBACK_QUERY}"), "-F", &format!("owner={owner}"), "-F", &format!("name={name}"), "-F", &format!("n={}", pr.number)],
        )?;
        parse_feedback(&json(&out)?).ok_or_else(|| IntError::Parse("no review threads in the answer".into()))
    }

    fn failed_ci_log(&self, root: &Path, branch: &str) -> Result<(String, String), IntError> {
        failed_log(root, branch)
    }

    fn create_pr(&self, root: &Path, head: &str, base: &str, title: &str, body: &str) -> Result<String, IntError> {
        let head = fork_head(root, head);
        let out = gh_run(root, &["pr", "create", "--head", &head, "--base", base, "--title", title, "--body", body])?;
        let out = String::from_utf8_lossy(&out);
        Ok(out.lines().rev().find(|l| l.starts_with("http")).unwrap_or(out.trim()).to_string())
    }

    fn existing_pr(&self, root: &Path, head: &str) -> Result<Option<String>, IntError> {
        match gh_run(root, &["pr", "view", &fork_head(root, head), "--json", "url", "-q", ".url"]) {
            Ok(out) => Ok(Some(String::from_utf8_lossy(&out).trim().to_string()).filter(|u| !u.is_empty())),
            Err(IntError::NotFound) => Ok(None),
            // `gh pr view` of a branch without one says "no pull requests found".
            Err(IntError::Network(said)) if said.contains("no pull requests") => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn merged_and_open(&self, root: &Path) -> Result<(Vec<ListedPr>, Vec<ListedPr>), IntError> {
        let list = |state: &str, mine: &[&str]| -> Result<Vec<ListedPr>, IntError> {
            let out = gh_run(root, &[&["pr", "list", "--state", state], mine, &["--limit", "300", "--json", LIST_FIELDS]].concat())?;
            Ok(crate::merged_branches::parse_prs(&String::from_utf8_lossy(&out)))
        };
        let merged = list("merged", &["--author", "@me"])?;
        if merged.is_empty() {
            return Ok((merged, Vec::new()));
        }
        Ok((merged, list("open", &[])?))
    }

    fn issue(&self, root: &Path, number: u64) -> Result<(String, String), IntError> {
        let v = json(&gh_run(root, &["issue", "view", &number.to_string(), "--json", "title,body"])?)?;
        Ok((v["title"].as_str().ok_or(IntError::NotFound)?.to_string(), v["body"].as_str().unwrap_or_default().to_string()))
    }

    fn open_requests(&self, root: &Path) -> Result<Vec<super::OpenRequest>, IntError> {
        let listed = json(&gh_run(root, &["pr", "list", "--state", "open", "--limit", "200", "--json", "number,title,url,headRefName,author,isCrossRepository,isDraft"])?)?;
        // Yours and those asking your review (a team's of yours too); not told, none.
        let status = gh_run(root, &["pr", "status", "--json", "number"]).ok().and_then(|out| json(&out).ok()).unwrap_or_default();
        Ok(parse_open(&listed, &status))
    }
}

/// `gh pr list` of open requests, marked by `gh pr status`: `createdBy`
/// yours, `needsReview` those asking your review.
fn parse_open(listed: &Value, status: &Value) -> Vec<super::OpenRequest> {
    let numbers = |key: &str| status[key].as_array().into_iter().flatten().filter_map(|v| v["number"].as_u64()).collect::<Vec<_>>();
    let (mine, for_me) = (numbers("createdBy"), numbers("needsReview"));
    listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let number = v["number"].as_u64()?;
            Some(super::OpenRequest {
                number,
                title: v["title"].as_str().unwrap_or_default().to_string(),
                url: v["url"].as_str().unwrap_or_default().to_string(),
                head: v["headRefName"].as_str()?.to_string(),
                author: v["author"]["login"].as_str().unwrap_or_default().to_string(),
                cross: v["isCrossRepository"].as_bool().unwrap_or(false),
                draft: v["isDraft"].as_bool().unwrap_or(false),
                for_me: for_me.contains(&number),
                mine: mine.contains(&number),
            })
        })
        .collect()
}

const LIST_FIELDS: &str = "number,title,headRefName,headRefOid,mergedAt,headRepository,headRepositoryOwner,isCrossRepository";

/// `gh` for the repository of `root`, run here, where the user signed in
/// to it. A remote task's repository is named (`GH_REPO`) as its origin
/// there names it, gh having no checkout here to read it from. Off the UI
/// thread: for a remote one it asks its host.
pub fn gh_for(root: &Path) -> Option<std::process::Command> {
    let mut gh = crate::procs::command("gh");
    if crate::hosts::is_remote(root) {
        gh.env("GH_REPO", github_repo(root)?).current_dir(std::env::temp_dir());
    } else {
        gh.current_dir(root);
    }
    Some(gh)
}

/// The head of a request made from `root`: its branch is pushed to origin,
/// the request goes where `request_remote` is (upstream before origin).
/// Origin a fork of that one: `owner:branch`, as GitHub reads a fork's head.
fn fork_head(root: &Path, branch: &str) -> String {
    let origin = crate::git::run(root, "git", &["remote", "get-url", "origin"]).ok().and_then(|u| super::remote_path(&u));
    let into = super::request_remote(root).and_then(|u| super::remote_path(&u));
    match (origin, into) {
        (Some(origin), Some(into)) if !origin.eq_ignore_ascii_case(&into) => head_of_fork(&origin, branch),
        _ => branch.to_string(),
    }
}

fn head_of_fork(origin_path: &str, branch: &str) -> String {
    match origin_path.split('/').next() {
        Some(owner) if !owner.is_empty() => format!("{owner}:{branch}"),
        _ => branch.to_string(),
    }
}

/// `HOST/OWNER/REPO` of `root`'s origin, as gh takes it: asked of the
/// repository once found, and kept while the app runs.
fn github_repo(root: &Path) -> Option<String> {
    use std::{collections::HashMap, sync::Mutex};
    static KNOWN: Mutex<Option<HashMap<PathBuf, String>>> = Mutex::new(None);
    if let Some(known) = KNOWN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(root) {
        return Some(known.clone());
    }
    // Not kept when not found: an origin may be added, a link come back.
    let found = super::request_remote(root).and_then(|url| repo_of_url(url.trim()))?;
    KNOWN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(root.to_path_buf(), found.clone());
    Some(found)
}

/// `github.com/owner/repo` of a remote's URL: `git@github.com:owner/repo.git`,
/// `https://github.com/owner/repo`, `ssh://git@host:22/owner/repo.git`.
fn repo_of_url(url: &str) -> Option<String> {
    let rest = url.strip_suffix('/').unwrap_or(url);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (host, path) = match rest.split_once("://") {
        Some((_, rest)) => {
            let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
            let (host, path) = rest.split_once('/')?;
            (host.split(':').next()?, path)
        }
        None => {
            let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
            rest.split_once(':')?
        }
    };
    let mut parts = path.trim_start_matches('/').split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (!host.is_empty() && !owner.is_empty() && !name.is_empty() && parts.next().is_none()).then(|| format!("{host}/{owner}/{name}"))
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
fn lookup_all(root: &Path, branches: &[String]) -> Result<Vec<Option<Pr>>, IntError> {
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
        // gh puts the repository of this directory in their place.
        let answer = json(&gh_run(root, &["api", "graphql", "-F", "owner={owner}", "-F", "name={repo}", "-f", &format!("query={query}")])?)?;
        let repository = answer["data"]["repository"].as_object().ok_or(IntError::NotFound)?;
        found.extend((0..chunk.len()).map(|i| parse_listed(&repository.get(&format!("b{i}"))?["nodes"][0])));
    }
    Ok(found)
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

/// "owner", "repo" from a PR's URL.
fn owner_repo(url: &str) -> Option<(String, String)> {
    let rest = url.split("github.com/").nth(1)?;
    let mut parts = rest.split('/');
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

const FEEDBACK_QUERY: &str = "query($owner:String!,$name:String!,$n:Int!){repository(owner:$owner,name:$name){pullRequest(number:$n){\
reviewThreads(first:100){nodes{isResolved isOutdated path line originalLine comments(first:30){nodes{body author{login} diffHunk}}}}\
reviews(last:50){nodes{state body author{login}}}}}}";

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


/// The latest failed run of `branch`'s workflows: its address, and the end
/// of the log of the steps that failed. Run off the UI thread.
fn failed_log(root: &Path, branch: &str) -> Result<(String, String), IntError> {
    let gh = |args: &[&str]| gh_run(root, args).map(|out| String::from_utf8_lossy(&out).into_owned());
    let runs: Value = serde_json::from_str(&gh(&["run", "list", "--branch", branch, "--limit", "20", "--json", "databaseId,conclusion,url"])?).map_err(|e| IntError::Parse(e.to_string()))?;
    let failed = runs
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| matches!(r["conclusion"].as_str(), Some("failure" | "timed_out" | "startup_failure")))
        .ok_or_else(|| IntError::Network(format!("No failed run of {branch} found")))?;
    let id = failed["databaseId"].as_u64().ok_or_else(|| IntError::Parse("a run without an id".into()))?.to_string();
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
    crate::pr::log_digest(&lines.join("\n"))
}

#[cfg(test)]
mod tests {
    /// Open requests as `gh` lists them, yours and those asking your review marked.
    #[test]
    fn open_requests_are_marked_yours_or_asking_you() {
        let listed: serde_json::Value = serde_json::from_str(r#"[
            {"number":7,"title":"Fix login","url":"https://github.com/o/r/pull/7","headRefName":"DEV-7_fixLogin","author":{"login":"ann"},"isCrossRepository":false,"isDraft":false},
            {"number":8,"title":"Mine","url":"https://github.com/o/r/pull/8","headRefName":"mine","author":{"login":"me"},"isCrossRepository":false,"isDraft":true},
            {"number":9,"title":"From a fork","url":"https://github.com/o/r/pull/9","headRefName":"main","author":{"login":"bob"},"isCrossRepository":true,"isDraft":false}
        ]"#).unwrap();
        let status: serde_json::Value = serde_json::from_str(r#"{"createdBy":[{"number":8}],"needsReview":[{"number":7}],"currentBranch":null}"#).unwrap();
        let open = super::parse_open(&listed, &status);
        assert_eq!(open.iter().map(|r| (r.number, r.for_me, r.mine, r.draft, r.cross)).collect::<Vec<_>>(), [(7, true, false, false, false), (8, false, true, true, false), (9, false, false, false, true)]);
        assert_eq!((open[0].head.as_str(), open[0].author.as_str(), open[0].url.as_str()), ("DEV-7_fixLogin", "ann", "https://github.com/o/r/pull/7"));
        assert!(super::parse_open(&listed, &serde_json::Value::Null).iter().all(|r| !r.for_me && !r.mine), "no status: nothing marked");
    }

    #[test]
    fn a_fork_s_head_names_its_owner() {
        assert_eq!(super::head_of_fork("me/app", "fix"), "me:fix");
        assert_eq!(super::head_of_fork("", "fix"), "fix");
    }

    use super::{parse, parse_feedback};
    use crate::pr::{Checks, PrState};

    #[test]
    fn a_remote_s_url_names_its_github_repository() {
        assert_eq!(super::repo_of_url("git@github.com:owner/repo.git").as_deref(), Some("github.com/owner/repo"));
        assert_eq!(super::repo_of_url("https://github.com/owner/repo").as_deref(), Some("github.com/owner/repo"));
        assert_eq!(super::repo_of_url("https://user@github.com/owner/repo.git/").as_deref(), Some("github.com/owner/repo"));
        assert_eq!(super::repo_of_url("ssh://git@ghe.example.com:22/team/app.git").as_deref(), Some("ghe.example.com/team/app"));
        assert_eq!(super::repo_of_url("/srv/git/repo"), None);
    }

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
        let one = |branch: &String| super::lookup_all(root, std::slice::from_ref(branch)).ok().and_then(|mut v| v.pop().flatten());
        assert_eq!(all.len(), branches.len());
        for (branch, pr) in branches.iter().zip(&all) {
            println!("{branch}: {pr:?}");
            assert_eq!(*pr, one(branch), "{branch}");
        }
    }

    /// Needs gh and the network: `cargo test live_feedback -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_feedback() {
        let pr = super::Pr { number: 13400, url: "https://github.com/cli/cli/pull/13400".into(), state: PrState::Open, checks: Checks::None, open_feedback: 0 };
        let f = super::CodeHost::feedback(&super::GitHub, std::path::Path::new("."), &pr).expect("gh answered");
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
