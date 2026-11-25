//! GitLab as a code host and its issues, through `glab api`: run here,
//! where the user signed in (`glab auth login`, `--hostname` for a server
//! of its own). Projects by their whole path, groups in groups included
//! (`group/sub/project`). A fork's merge requests live in its upstream:
//! asked there, made from origin into it.

use std::path::Path;

use serde_json::Value;

use super::{CodeHost, IntError, ListedPr};
use crate::pr::{Checks, Feedback, Pr, PrState, Thread};

pub struct GitLab {
    /// What runs `glab`: the program, or a stand-in in the tests.
    pub program: String,
    pub host: String,
    /// The project requests are asked of: upstream, else origin.
    pub path: String,
}

impl GitLab {
    pub fn for_remote(url: &str) -> Result<Self, IntError> {
        let path = super::remote_path(url).ok_or_else(|| IntError::NoRepository(format!("not a GitLab project's address: {url}")))?;
        Ok(Self { program: "glab".into(), host: super::remote_host(url), path })
    }

    fn id(path: &str) -> String {
        path.replace('/', "%2F")
    }

    /// `glab api` of an endpoint (`projects/…`) or `graphql`, with fields.
    fn api(&self, method: &str, endpoint: &str, fields: &[(&str, &str)]) -> Result<Value, IntError> {
        let mut command = crate::procs::command(&self.program);
        command.args(["api", "--hostname", &self.host, "--method", method, endpoint]);
        for (k, v) in fields {
            command.arg("-f").arg(format!("{k}={v}"));
        }
        let out = command
            .env("GLAB_NO_PROMPT", "1")
            .env("NO_PROMPT", "1")
            .current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { IntError::ToolMissing("glab") } else { IntError::Network(format!("glab: {e}")) })?;
        if !out.status.success() {
            let said = format!("{}{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
            return Err(IntError::from_cli("glab", &format!("glab auth login --hostname {}", self.host), &said));
        }
        // A job's trace is text, not JSON.
        Ok(serde_json::from_slice(&out.stdout).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&out.stdout).into_owned())))
    }

    fn get(&self, endpoint: &str) -> Result<Value, IntError> {
        self.api("GET", endpoint, &[])
    }

    fn project(&self) -> String {
        format!("projects/{}", Self::id(&self.path))
    }
}

/// Branches asked of GitLab in one query.
const BRANCHES_A_QUERY: usize = 50;

impl CodeHost for GitLab {
    fn prs_for_branches(&self, _root: &Path, branches: &[String]) -> Result<Vec<Option<Pr>>, IntError> {
        let mut found = Vec::with_capacity(branches.len());
        for chunk in branches.chunks(BRANCHES_A_QUERY) {
            let asked: Vec<&String> = chunk.iter().filter(|b| !b.is_empty() && !b.starts_with('(')).collect();
            if asked.is_empty() {
                found.extend(chunk.iter().map(|_| None));
                continue;
            }
            let query = format!(
                "query{{project(fullPath:{}){{mergeRequests(sourceBranches:{},first:100,sort:CREATED_DESC){{nodes{{iid webUrl state draft sourceBranch headPipeline{{status}} discussions(first:100){{nodes{{resolvable resolved}}}}}}}}}}}}",
                serde_json::to_string(&self.path).unwrap_or_default(),
                serde_json::to_string(&asked).unwrap_or_default()
            );
            let answer = self.api("POST", "graphql", &[("query", &query)])?;
            let project = &answer["data"]["project"];
            if project.is_null() {
                return Err(IntError::NotFound);
            }
            let nodes = project["mergeRequests"]["nodes"].as_array().cloned().unwrap_or_default();
            // Newest first: the first of a branch is its request.
            found.extend(chunk.iter().map(|branch| nodes.iter().find(|n| n["sourceBranch"].as_str() == Some(branch.as_str())).and_then(parse_listed)));
        }
        Ok(found)
    }

    fn pr(&self, _root: &Path, number: u64) -> Result<(Pr, String), IntError> {
        let v = self.get(&format!("{}/merge_requests/{number}", self.project()))?;
        let pr = parse_rest(&v).ok_or_else(|| IntError::Parse("a merge request without its number".into()))?;
        Ok((pr, v["source_branch"].as_str().unwrap_or_default().to_string()))
    }

    fn request_info(&self, _root: &Path, number: u64) -> Result<super::RequestInfo, IntError> {
        let v = self.get(&format!("{}/merge_requests/{number}", self.project()))?;
        Ok(super::RequestInfo {
            pr: parse_rest(&v).ok_or_else(|| IntError::Parse("a merge request without its number".into()))?,
            head: v["source_branch"].as_str().unwrap_or_default().to_string(),
            title: v["title"].as_str().unwrap_or_default().to_string(),
            cross: v["source_project_id"].as_u64() != v["target_project_id"].as_u64(),
        })
    }

    fn feedback(&self, _root: &Path, pr: &Pr) -> Result<Feedback, IntError> {
        let v = self.get(&format!("{}/merge_requests/{}/discussions?per_page=100", self.project(), pr.number))?;
        Ok(parse_discussions(&v))
    }

    fn failed_ci_log(&self, _root: &Path, branch: &str) -> Result<(String, String), IntError> {
        let pipelines = self.get(&format!("{}/pipelines?ref={}&per_page=20", self.project(), query_value(branch)))?;
        let failed = pipelines
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["status"].as_str() == Some("failed"))
            .ok_or_else(|| IntError::Network(format!("No failed pipeline of {branch} found")))?;
        let id = failed["id"].as_u64().ok_or_else(|| IntError::Parse("a pipeline without an id".into()))?;
        let url = failed["web_url"].as_str().unwrap_or_default().to_string();
        let jobs = self.get(&format!("{}/pipelines/{id}/jobs?scope[]=failed&per_page=100", self.project()))?;
        let mut log = String::new();
        for job in jobs.as_array().into_iter().flatten().take(5) {
            let Some(job_id) = job["id"].as_u64() else { continue };
            let trace = self.get(&format!("{}/jobs/{job_id}/trace", self.project()))?;
            log.push_str(&format!("== {} ({})\n", job["name"].as_str().unwrap_or("job"), job["stage"].as_str().unwrap_or_default()));
            log.push_str(&without_sections(trace.as_str().unwrap_or_default()));
            log.push('\n');
        }
        Ok((url, crate::pr::log_digest(&log)))
    }

    fn create_pr(&self, root: &Path, head: &str, base: &str, title: &str, body: &str) -> Result<String, IntError> {
        // A fork's: made in origin, into upstream.
        let origin = crate::git::run(root, "git", &["remote", "get-url", "origin"]).ok().and_then(|u| super::remote_path(u.trim()));
        let source = origin.clone().unwrap_or_else(|| self.path.clone());
        let mut fields = vec![("source_branch", head.to_string()), ("target_branch", base.to_string()), ("title", title.to_string()), ("description", body.to_string())];
        if source != self.path {
            let target = self.get(&self.project())?;
            let id = target["id"].as_u64().ok_or(IntError::NotFound)?;
            fields.push(("target_project_id", id.to_string()));
        }
        let fields: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let made = self.api("POST", &format!("projects/{}/merge_requests", Self::id(&source)), &fields)?;
        made["web_url"].as_str().map(str::to_string).ok_or_else(|| IntError::Parse("no address of the merge request made".into()))
    }

    fn existing_pr(&self, _root: &Path, head: &str) -> Result<Option<String>, IntError> {
        let v = self.get(&format!("{}/merge_requests?state=opened&source_branch={}", self.project(), query_value(head)))?;
        Ok(v.as_array().and_then(|all| all.first()).and_then(|mr| mr["web_url"].as_str()).map(str::to_string))
    }

    fn merged_and_open(&self, _root: &Path) -> Result<(Vec<ListedPr>, Vec<ListedPr>), IntError> {
        let list = |state: &str| -> Result<Vec<ListedPr>, IntError> {
            let scope = if state == "merged" { "&scope=created_by_me" } else { "" };
            let mut all = Vec::new();
            for page in 1..=3 {
                let v = self.get(&format!("{}/merge_requests?state={state}{scope}&per_page=100&page={page}&order_by=updated_at", self.project()))?;
                let got = parse_listed_rest(&v);
                let full = got.len() == 100;
                all.extend(got);
                if !full {
                    break;
                }
            }
            Ok(all)
        };
        let merged = list("merged")?;
        if merged.is_empty() {
            return Ok((merged, Vec::new()));
        }
        Ok((merged, list("opened")?))
    }

    fn issue(&self, _root: &Path, number: u64) -> Result<(String, String), IntError> {
        let v = self.get(&format!("{}/issues/{number}", self.project()))?;
        Ok((v["title"].as_str().ok_or(IntError::NotFound)?.to_string(), v["description"].as_str().unwrap_or_default().to_string()))
    }
}

/// A value put in a query string: what is not a letter, digit or `-._~` escaped.
fn query_value(text: &str) -> String {
    text.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn checks(status: Option<&str>) -> Checks {
    match status.map(str::to_ascii_lowercase).as_deref() {
        None | Some("") => Checks::None,
        Some("failed" | "canceled" | "canceling") => Checks::Failed,
        Some("success" | "skipped") => Checks::Passed,
        Some(_) => Checks::Pending,
    }
}

fn state(word: &str, draft: bool) -> PrState {
    match word.to_ascii_lowercase().as_str() {
        "merged" => PrState::Merged,
        "closed" | "locked" => PrState::Closed,
        _ if draft => PrState::Draft,
        _ => PrState::Open,
    }
}

/// A merge request as the GraphQL query of branches tells of it.
fn parse_listed(v: &Value) -> Option<Pr> {
    let number = v["iid"].as_str().and_then(|n| n.parse().ok()).or(v["iid"].as_u64())?;
    let state = state(v["state"].as_str()?, v["draft"].as_bool() == Some(true));
    let open_feedback = if matches!(state, PrState::Open | PrState::Draft) {
        v["discussions"]["nodes"].as_array().into_iter().flatten().filter(|d| d["resolvable"].as_bool() == Some(true) && d["resolved"].as_bool() != Some(true)).count()
    } else {
        0
    };
    Some(Pr { number, url: v["webUrl"].as_str()?.to_string(), state, checks: checks(v["headPipeline"]["status"].as_str()), open_feedback })
}

/// One as the REST API tells of it.
fn parse_rest(v: &Value) -> Option<Pr> {
    Some(Pr {
        number: v["iid"].as_u64()?,
        url: v["web_url"].as_str()?.to_string(),
        state: state(v["state"].as_str()?, v["draft"].as_bool() == Some(true) || v["work_in_progress"].as_bool() == Some(true)),
        checks: checks(v["head_pipeline"]["status"].as_str().or(v["pipeline"]["status"].as_str())),
        open_feedback: 0,
    })
}

fn parse_listed_rest(v: &Value) -> Vec<ListedPr> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|mr| {
            Some(ListedPr {
                head: mr["source_branch"].as_str()?.to_string(),
                head_oid: mr["sha"].as_str().unwrap_or_default().to_string(),
                number: mr["iid"].as_u64()?,
                title: mr["title"].as_str().unwrap_or_default().to_string(),
                merged_at: mr["merged_at"].as_str().unwrap_or_default().to_string(),
                head_repo: None,
                cross: mr["source_project_id"].as_u64() != mr["target_project_id"].as_u64(),
            })
        })
        .collect()
}

/// The discussions not resolved: on a line of the diff, a thread; on the
/// request as a whole, a review's words.
fn parse_discussions(v: &Value) -> Feedback {
    let mut feedback = Feedback::default();
    for d in v.as_array().into_iter().flatten() {
        let notes: Vec<&Value> = d["notes"].as_array().into_iter().flatten().filter(|n| n["system"].as_bool() != Some(true)).collect();
        let Some(first) = notes.first() else { continue };
        if first["resolvable"].as_bool() != Some(true) || first["resolved"].as_bool() == Some(true) {
            continue;
        }
        let author = |n: &Value| n["author"]["username"].as_str().unwrap_or("someone").to_string();
        let position = &first["position"];
        match position["new_path"].as_str().or(position["old_path"].as_str()) {
            Some(path) => feedback.threads.push(Thread {
                path: path.to_string(),
                line: position["new_line"].as_u64().or(position["old_line"].as_u64()),
                // GitLab tells it of the position, not of the thread: a diff since is not known here.
                outdated: false,
                hunk: String::new(),
                comments: notes.iter().map(|n| (author(n), n["body"].as_str().unwrap_or_default().to_string())).collect(),
            }),
            None => {
                let words = notes.iter().map(|n| n["body"].as_str().unwrap_or_default().trim().to_string()).collect::<Vec<_>>().join("\n");
                feedback.reviews.push((author(first), words));
            }
        }
    }
    feedback
}

/// A job's trace without GitLab's section markers
/// (`section_start:1700000000:step_script\r\x1b[0K`): what is left is the
/// output, colours taken off later.
fn without_sections(trace: &str) -> String {
    trace
        .lines()
        .map(|line| {
            let mut line = line.to_string();
            for marker in ["section_start:", "section_end:"] {
                while let Some(at) = line.find(marker) {
                    // `section_start:<time>:<name>[options]` ends at the carriage return or the escape that follows.
                    let rest = &line[at..];
                    let end = rest.find(['\r', '\x1b']).map_or(rest.len(), |e| e);
                    line.replace_range(at..at + end, "");
                }
            }
            line.replace("\x1b[0K", "").replace('\r', "")
        })
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_in_groups_of_groups() {
        let gl = GitLab::for_remote("ssh://git@gitlab.example.com:2222/group/sub/app.git").unwrap();
        assert_eq!((gl.host.as_str(), gl.path.as_str(), gl.project()), ("gitlab.example.com", "group/sub/app", "projects/group%2Fsub%2Fapp".to_string()));
        assert!(GitLab::for_remote("/srv/app").is_err());
        assert_eq!(query_value("feature/DEV-1 ä"), "feature%2FDEV-1%20%C3%A4");
    }

    #[test]
    fn merge_requests_of_branches() {
        let v: Value = serde_json::from_str(
            r#"{"iid":"12","webUrl":"https://gitlab.example.com/g/s/a/-/merge_requests/12","state":"opened","draft":true,"sourceBranch":"fix",
            "headPipeline":{"status":"FAILED"},"discussions":{"nodes":[{"resolvable":true,"resolved":false},{"resolvable":true,"resolved":true},{"resolvable":false,"resolved":false}]}}"#,
        )
        .unwrap();
        let pr = parse_listed(&v).unwrap();
        assert_eq!((pr.number, pr.state, pr.checks, pr.open_feedback), (12, PrState::Draft, Checks::Failed, 1));
        assert_eq!(pr.label(), "#12 draft");
        let merged: Value = serde_json::from_str(r#"{"iid":"3","webUrl":"u","state":"merged","draft":false,"headPipeline":null}"#).unwrap();
        assert_eq!(parse_listed(&merged).map(|pr| (pr.state, pr.checks)), Some((PrState::Merged, Checks::None)));
        assert_eq!(checks(Some("RUNNING")), Checks::Pending);
        assert_eq!(checks(Some("SUCCESS")), Checks::Passed);
    }

    #[test]
    fn unresolved_discussions_are_its_feedback() {
        let v: Value = serde_json::from_str(
            r#"[
            {"notes":[{"system":true,"body":"added 1 commit","resolvable":false}]},
            {"notes":[{"resolvable":true,"resolved":false,"body":"rename this","author":{"username":"rev"},"position":{"new_path":"src/a.rs","new_line":7,"old_path":"src/a.rs","old_line":null}},
                      {"resolvable":true,"resolved":false,"body":"agreed","author":{"username":"me"}}]},
            {"notes":[{"resolvable":true,"resolved":true,"body":"done","author":{"username":"rev"},"position":{"new_path":"b.rs","new_line":1}}]},
            {"notes":[{"resolvable":true,"resolved":false,"body":"Needs tests","author":{"username":"lead"}}]}
        ]"#,
        )
        .unwrap();
        let f = parse_discussions(&v);
        assert_eq!(f.len(), 2);
        assert_eq!((f.threads[0].path.as_str(), f.threads[0].line, f.threads[0].comments.len()), ("src/a.rs", Some(7), 2));
        assert_eq!(f.reviews, vec![("lead".to_string(), "Needs tests".to_string())]);
    }

    #[test]
    fn merged_requests_listed() {
        let v: Value = serde_json::from_str(
            r#"[{"iid":5,"title":"Fix","source_branch":"fix","sha":"abc","merged_at":"2026-11-17T10:00:00Z","source_project_id":1,"target_project_id":1},
               {"iid":6,"title":"From a fork","source_branch":"main","sha":"def","merged_at":"2026-11-18T10:00:00Z","source_project_id":2,"target_project_id":1}]"#,
        )
        .unwrap();
        let all = parse_listed_rest(&v);
        assert_eq!(all.iter().map(|p| (p.head.as_str(), p.number, p.cross)).collect::<Vec<_>>(), [("fix", 5, false), ("main", 6, true)]);
    }

    #[test]
    fn a_trace_without_its_sections() {
        let trace = "\x1b[0Ksection_start:1700000000:step_script\r\x1b[0K\x1b[0K\x1b[36;1mExecuting \"step_script\"\x1b[0;m\nerror: it broke\nsection_end:1700000001:step_script\r\x1b[0K\n";
        let plain = without_sections(trace);
        assert!(!plain.contains("section_"), "{plain:?}");
        assert!(plain.contains("error: it broke"));
    }

    /// A stand-in `glab` answers from files: the request of a branch and
    /// the log of its failed pipeline go through the program as they would.
    #[cfg(unix)]
    #[test]
    fn a_stand_in_glab_is_asked() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("ideae-glab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("glab");
        // The endpoint is the sixth word: api --hostname H --method M ENDPOINT.
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$6" in
  graphql) echo '{"data":{"project":{"mergeRequests":{"nodes":[{"iid":"9","webUrl":"https://gitlab.example.com/g/s/a/-/merge_requests/9","state":"opened","draft":false,"sourceBranch":"fix","headPipeline":{"status":"SUCCESS"},"discussions":{"nodes":[]}}]}}}}' ;;
  *pipelines\?ref=*) echo '[{"id":77,"status":"failed","web_url":"https://gitlab.example.com/g/s/a/-/pipelines/77"}]' ;;
  */pipelines/77/jobs*) echo '[{"id":5,"name":"test","stage":"test"}]' ;;
  */jobs/5/trace) printf 'compiling\nerror[E0308]: mismatched types\n' ;;
  *) echo "404 Not Found" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let gl = GitLab { program: script.display().to_string(), host: "gitlab.example.com".into(), path: "g/s/a".into() };
        let root = Path::new("/nowhere");
        let prs = gl.prs_for_branches(root, &["fix".into(), "other".into(), String::new()]).unwrap();
        assert_eq!(prs.iter().map(|p| p.as_ref().map(|p| (p.number, p.checks))).collect::<Vec<_>>(), [Some((9, Checks::Passed)), None, None]);
        let (url, log) = gl.failed_ci_log(root, "fix").unwrap();
        assert!(url.ends_with("/pipelines/77"));
        assert!(log.contains("mismatched types"), "{log}");
        assert_eq!(gl.issue(root, 1), Err(IntError::NotFound));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Against a real GitLab, signed in with glab: `IDEAE_LIVE_GITLAB=host/group/project`
    /// and `IDEAE_LIVE_GITLAB_BRANCHES=a,b`, then
    /// `cargo test a_real_gitlab -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn a_real_gitlab() {
        let Ok(at) = std::env::var("IDEAE_LIVE_GITLAB") else { return };
        let gl = GitLab::for_remote(&format!("https://{at}")).expect("host/group/project");
        let branches: Vec<String> = std::env::var("IDEAE_LIVE_GITLAB_BRANCHES").unwrap_or_else(|_| "main".into()).split(',').map(str::to_string).collect();
        let root = Path::new(".");
        let prs = gl.prs_for_branches(root, &branches).expect("glab answered");
        for (branch, pr) in branches.iter().zip(&prs) {
            println!("{branch}: {pr:?}");
            if let Some(pr) = pr {
                println!("{}", gl.feedback(root, pr).expect("discussions").message(pr));
            }
        }
        let (merged, open) = gl.merged_and_open(root).expect("lists");
        println!("{} merged, {} open", merged.len(), open.len());
    }
}
