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
    parse(&serde_json::from_slice(&out.stdout).ok()?)
}

fn parse(v: &Value) -> Option<Pr> {
    let state = match v["state"].as_str()? {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ if v["isDraft"].as_bool() == Some(true) => PrState::Draft,
        _ => PrState::Open,
    };
    Some(Pr { number: v["number"].as_u64()?, url: v["url"].as_str()?.to_string(), state, checks: checks(&v["statusCheckRollup"]) })
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
    use super::{Checks, PrState, parse};

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
}
