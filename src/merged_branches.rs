//! Branches left on `origin` after their pull request was merged: which
//! ones can go without losing anything, and deleting them. GitHub keeps
//! the merged PR and its commits; only the branch goes.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use serde_json::Value;

/// A remote branch whose PR was merged, safe to delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedBranch {
    pub branch: String,
    pub number: u64,
    pub title: String,
    /// `2026-11-17`.
    pub merged: String,
}

/// A PR as `gh pr list` tells of it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListedPr {
    pub head: String,
    /// The head commit as it was merged: a squash or rebase merge leaves
    /// the branch's commits out of the default branch, this one says they
    /// are what was merged.
    pub head_oid: String,
    pub number: u64,
    pub title: String,
    /// ISO 8601, as GitHub gives it; empty for one not merged.
    pub merged_at: String,
    /// "owner/name" of the repository its head is in, lowercase.
    pub head_repo: Option<String>,
    pub cross: bool,
}

/// A branch of `origin` as the remote-tracking refs have it.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteBranch {
    pub name: String,
    pub tip: String,
    /// Its tip's committer time, seconds since 1970.
    pub time: u64,
    /// Its tip is in the default branch.
    pub in_default: bool,
}

pub fn parse_prs(json: &str) -> Vec<ListedPr> {
    let Ok(Value::Array(prs)) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    prs.iter()
        .filter_map(|v| {
            let owner = v["headRepositoryOwner"]["login"].as_str();
            let name = v["headRepository"]["name"].as_str();
            Some(ListedPr {
                head: v["headRefName"].as_str()?.to_string(),
                head_oid: v["headRefOid"].as_str().unwrap_or_default().to_string(),
                number: v["number"].as_u64()?,
                title: v["title"].as_str().unwrap_or_default().to_string(),
                merged_at: v["mergedAt"].as_str().unwrap_or_default().to_string(),
                head_repo: owner.zip(name).map(|(o, n)| format!("{o}/{n}").to_lowercase()),
                cross: v["isCrossRepository"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

/// "owner/name" of a GitHub remote's URL, lowercase: `git@github.com:o/r.git`,
/// `https://github.com/o/r`, `ssh://git@host/o/r.git`.
pub fn owner_repo(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => url.split_once(':').filter(|(host, _)| !host.contains('/') && host.len() > 1)?.1,
    };
    let path = path.trim_end_matches(".git");
    let mut parts = path.rsplit('/');
    let (name, owner) = (parts.next()?, parts.next()?);
    (!name.is_empty() && !owner.is_empty()).then(|| format!("{owner}/{name}").to_lowercase())
}

/// Whether the PR's head is a branch of `origin`: its repository is
/// origin's, or, when origin's is not known, the PR is not from a fork.
fn of_origin(pr: &ListedPr, origin: Option<&str>) -> bool {
    match (origin, &pr.head_repo) {
        (Some(origin), Some(head)) => origin == head,
        _ => !pr.cross,
    }
}

/// The branches that can go. A branch of `origin` goes when the latest
/// merged PR of its name has its head there, and its tip is that PR's
/// head as merged, or is in the default branch and no newer than the
/// merge; not when it is the default branch, a task's, or the head of a
/// PR still open.
pub fn select(merged: &[ListedPr], open: &[ListedPr], branches: &[RemoteBranch], default: &str, in_use: &HashSet<String>, origin: Option<&str>) -> Vec<MergedBranch> {
    let open: HashSet<&str> = open.iter().filter(|pr| of_origin(pr, origin)).map(|pr| pr.head.as_str()).collect();
    // Each branch's latest merged PR, found once: not every PR for every branch.
    let mut latest: HashMap<&str, &ListedPr> = HashMap::new();
    for pr in merged.iter().filter(|pr| !pr.merged_at.is_empty() && of_origin(pr, origin)) {
        let at = latest.entry(pr.head.as_str()).or_insert(pr);
        if pr.merged_at > at.merged_at {
            *at = pr;
        }
    }
    let mut out: Vec<MergedBranch> = branches
        .iter()
        .filter(|b| b.name != default && b.name != "HEAD" && !in_use.contains(&b.name) && !open.contains(b.name.as_str()))
        .filter_map(|b| {
            let pr = *latest.get(b.name.as_str())?;
            let as_merged = !pr.head_oid.is_empty() && pr.head_oid == b.tip;
            // Seconds of the two, as text: "2026-11-17T10:00:00".
            let tip_time = crate::usage::stamp(b.time);
            let not_newer = tip_time.get(..19) <= pr.merged_at.get(..19);
            (as_merged || (b.in_default && not_newer)).then(|| MergedBranch {
                branch: b.name.clone(),
                number: pr.number,
                title: pr.title.clone(),
                merged: pr.merged_at.get(..10).unwrap_or_default().to_string(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.merged.cmp(&a.merged).then(a.branch.cmp(&b.branch)));
    out
}

fn gh(repo: &Path, args: &[&str]) -> Option<String> {
    let out = crate::procs::command("gh").current_dir(repo).env("GH_PROMPT_DISABLED", "1").args(args).stdin(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

const FIELDS: &str = "number,title,headRefName,headRefOid,mergedAt,headRepository,headRepositoryOwner,isCrossRepository";

/// The merged branches of the project at `repo`, `in_use` left out: the
/// branches its tasks have checked out. Off the UI thread: asks GitHub.
pub fn find(repo: &Path, in_use: &HashSet<String>) -> Vec<MergedBranch> {
    let git = |args: &[&str]| crate::git::git_status(repo, args).ok();
    let Some(default) = crate::git::remote_default(repo) else { return Vec::new() };
    let Some(refs) = git(&["for-each-ref", "refs/remotes/origin", "--format=%(refname:strip=3)%00%(objectname)%00%(committerdate:unix)"]) else { return Vec::new() };
    if refs.lines().all(|line| line.starts_with("HEAD\0") || line.starts_with(&format!("{default}\0"))) {
        return Vec::new();
    }
    let Some(merged) = gh(repo, &["pr", "list", "--state", "merged", "--limit", "300", "--json", FIELDS]) else { return Vec::new() };
    let merged = parse_prs(&merged);
    if merged.is_empty() {
        return Vec::new();
    }
    // No remote branch is the head of a merged PR: GitHub need not be asked more.
    let heads: HashSet<&str> = merged.iter().map(|pr| pr.head.as_str()).collect();
    if !refs.lines().any(|line| line.split('\0').next().is_some_and(|name| heads.contains(name))) {
        return Vec::new();
    }
    let Some(open) = gh(repo, &["pr", "list", "--state", "open", "--limit", "300", "--json", FIELDS]) else { return Vec::new() };
    let in_default: HashSet<String> =
        git(&["for-each-ref", &format!("--merged=refs/remotes/origin/{default}"), "refs/remotes/origin", "--format=%(refname:strip=3)"]).unwrap_or_default().lines().map(str::to_string).collect();
    let branches: Vec<RemoteBranch> = refs
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\0');
            let name = parts.next()?.to_string();
            Some(RemoteBranch { in_default: in_default.contains(&name), tip: parts.next()?.to_string(), time: parts.next()?.trim().parse().ok()?, name })
        })
        .collect();
    let origin = git(&["remote", "get-url", "origin"]).and_then(|url| owner_repo(&url));
    select(&merged, &parse_prs(&open), &branches, &default, in_use, origin.as_deref())
}

/// What a deletion did, branch by branch.
#[derive(Debug, Default, PartialEq)]
pub struct Deleted {
    pub deleted: Vec<String>,
    /// The branch and why not.
    pub failed: Vec<(String, String)>,
}

/// Deletes `branches` from `origin` in one push, then drops the
/// remote-tracking refs of all branches gone from it. Off the UI thread.
pub fn delete(repo: &Path, branches: &[String]) -> Deleted {
    let git = || {
        let mut command = crate::procs::command("git");
        command.arg("-C").arg(repo).env("GIT_TERMINAL_PROMPT", "0").stdin(std::process::Stdio::null());
        command
    };
    // One branch gone from origin already fails the whole push: those
    // count as deleted, and are not asked for.
    let mut ls = vec!["ls-remote", "--heads", "origin"];
    ls.extend(branches.iter().map(String::as_str));
    let there: Option<HashSet<String>> = git().args(&ls).output().ok().filter(|o| o.status.success()).map(|o| {
        String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.split_once("refs/heads/").map(|(_, b)| b.trim().to_string())).collect()
    });
    let (asked, gone): (Vec<String>, Vec<String>) = branches.iter().cloned().partition(|b| there.as_ref().is_none_or(|there| there.contains(b)));
    let mut done = Deleted { deleted: gone, failed: Vec::new() };
    if !asked.is_empty() {
        let push = |asked: &[String]| {
            let mut args = vec!["push", "--porcelain", "origin", "--delete"];
            args.extend(asked.iter().map(String::as_str));
            match git().args(&args).output() {
                Ok(out) => parse_push(&String::from_utf8_lossy(&out.stdout), &String::from_utf8_lossy(&out.stderr), asked),
                Err(e) => Deleted { deleted: Vec::new(), failed: asked.iter().map(|b| (b.clone(), e.to_string())).collect() },
            }
        };
        let mut pushed = push(&asked);
        // A server that refuses the whole push for one protected branch:
        // the others once more, without it.
        let (protected, others): (Vec<_>, Vec<_>) = pushed.failed.iter().cloned().partition(|(_, why)| why.starts_with("protected"));
        if !protected.is_empty() && !others.is_empty() {
            let again = push(&others.into_iter().map(|(b, _)| b).collect::<Vec<_>>());
            pushed.deleted.extend(again.deleted);
            pushed.failed = protected.into_iter().chain(again.failed).collect();
        }
        done.deleted.extend(pushed.deleted);
        done.failed = pushed.failed;
    }
    let _ = crate::git::fetch_all(repo);
    done
}

/// `git push --porcelain` says per ref: "-\t:refs/heads/b\t[deleted]", or
/// "!\t:refs/heads/b\t[remote rejected] (reason)". A branch gone already
/// is told on stderr ("remote ref does not exist") and counts as deleted.
fn parse_push(stdout: &str, stderr: &str, asked: &[String]) -> Deleted {
    let mut done = Deleted::default();
    for line in stdout.lines() {
        let mut parts = line.split('\t');
        let (Some(flag), Some(refs), Some(summary)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Some(branch) = refs.rsplit(':').next().and_then(|r| r.strip_prefix("refs/heads/")) else { continue };
        if flag == "-" {
            done.deleted.push(branch.to_string());
        } else if flag == "!" {
            let reason = summary.split_once('(').map_or(summary, |(_, r)| r.trim_end_matches(')')).trim();
            let protected = reason.contains("protected");
            done.failed.push((branch.to_string(), if protected { format!("protected: {reason}") } else { reason.to_string() }));
        }
    }
    for branch in asked {
        if done.deleted.contains(branch) || done.failed.iter().any(|(b, _)| b == branch) {
            continue;
        }
        let gone = stderr.lines().any(|l| l.contains("remote ref does not exist") && l.contains(&format!("'{branch}'")));
        if gone {
            done.deleted.push(branch.clone());
        } else {
            let why = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("not deleted").trim().to_string();
            done.failed.push((branch.clone(), why));
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    const MERGED: &str = r#"[
        {"number":10,"title":"Squashed","headRefName":"squashed","headRefOid":"aaa","mergedAt":"2026-11-10T12:00:00Z","headRepository":{"name":"Repo"},"headRepositoryOwner":{"login":"Me"},"isCrossRepository":false},
        {"number":11,"title":"Merged in","headRefName":"merged-in","headRefOid":"old","mergedAt":"2026-11-11T12:00:00Z","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"me"},"isCrossRepository":false},
        {"number":12,"title":"Kept working","headRefName":"kept","headRefOid":"ccc","mergedAt":"2026-11-12T12:00:00Z","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"me"},"isCrossRepository":false},
        {"number":13,"title":"From a fork","headRefName":"forked","headRefOid":"ddd","mergedAt":"2026-11-12T12:00:00Z","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"other"},"isCrossRepository":true},
        {"number":14,"title":"Again","headRefName":"again","headRefOid":"eee","mergedAt":"2026-11-01T12:00:00Z","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"me"},"isCrossRepository":false},
        {"number":15,"title":"Task","headRefName":"task","headRefOid":"fff","mergedAt":"2026-11-01T12:00:00Z","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"me"},"isCrossRepository":false},
        {"number":16,"title":"Main","headRefName":"main","headRefOid":"ggg","mergedAt":"2026-11-01T12:00:00Z","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"me"},"isCrossRepository":false},
        {"number":17,"title":"Late","headRefName":"late","headRefOid":"hhh","mergedAt":"2026-11-13T12:00:00Z","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"me"},"isCrossRepository":false}
    ]"#;
    const OPEN: &str = r#"[{"number":20,"title":"Again, more","headRefName":"again","headRefOid":"x","mergedAt":null,"headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"me"},"isCrossRepository":false}]"#;

    fn at(iso: &str) -> u64 {
        // Seconds of a day in November 2026, for the tests' dates.
        let day: u64 = iso[8..10].parse().unwrap();
        let hour: u64 = iso[11..13].parse().unwrap();
        1_793_491_200 + (day - 1) * 86_400 + hour * 3600
    }

    fn branch(name: &str, tip: &str, time: &str, in_default: bool) -> RemoteBranch {
        RemoteBranch { name: name.into(), tip: tip.into(), time: at(time), in_default }
    }

    #[test]
    fn the_clock_of_the_tests() {
        assert_eq!(crate::usage::stamp(at("2026-11-10T12")), "2026-11-10T12:00:00.000Z");
    }

    #[test]
    fn which_branches_go() {
        let branches = [
            // Squash-merged: its tip is not in main, but is what was merged.
            branch("squashed", "aaa", "2026-11-10T11", false),
            // Merged with a merge commit: its tip is in main.
            branch("merged-in", "bbb", "2026-11-11T10", true),
            // A commit after the merge, in no PR.
            branch("kept", "c2", "2026-11-12T15", false),
            branch("forked", "ddd", "2026-11-12T10", false),
            // A new PR of the same branch is open.
            branch("again", "eee", "2026-11-01T10", false),
            // A task of the app has it checked out.
            branch("task", "fff", "2026-11-01T10", false),
            branch("main", "ggg", "2026-11-01T10", true),
            branch("HEAD", "ggg", "2026-11-01T10", true),
            // In main, but committed after the merge: someone went on.
            branch("late", "h2", "2026-11-14T10", true),
            // No PR at all.
            branch("lonely", "iii", "2026-11-01T10", true),
        ];
        let in_use: HashSet<String> = ["task".to_string()].into();
        let got = select(&parse_prs(MERGED), &parse_prs(OPEN), &branches, "main", &in_use, Some("me/repo"));
        let names: Vec<&str> = got.iter().map(|b| b.branch.as_str()).collect();
        assert_eq!(names, ["merged-in", "squashed"]);
        assert_eq!(got[1], MergedBranch { branch: "squashed".into(), number: 10, title: "Squashed".into(), merged: "2026-11-10".into() });
    }

    #[test]
    fn a_fork_is_told_by_its_repository() {
        // Origin is the fork: its PRs to upstream are cross-repository, yet their heads are origin's.
        let prs = parse_prs(MERGED);
        let branches = [branch("forked", "ddd", "2026-11-12T10", false)];
        let none = HashSet::new();
        assert_eq!(select(&prs, &[], &branches, "main", &none, Some("other/repo")).len(), 1);
        assert!(select(&prs, &[], &branches, "main", &none, Some("me/repo")).is_empty());
        // Origin's repository unknown (not on GitHub): forks are left out.
        assert!(select(&prs, &[], &branches, "main", &none, None).is_empty());
    }

    #[test]
    fn the_latest_merge_of_a_name_counts() {
        let json = r#"[
            {"number":1,"title":"First","headRefName":"b","headRefOid":"one","mergedAt":"2026-11-01T12:00:00Z","isCrossRepository":false},
            {"number":2,"title":"Second","headRefName":"b","headRefOid":"two","mergedAt":"2026-11-05T12:00:00Z","isCrossRepository":false}]"#;
        let prs = parse_prs(json);
        let none = HashSet::new();
        let got = select(&prs, &[], &[branch("b", "two", "2026-11-05T10", false)], "main", &none, None);
        assert_eq!(got.iter().map(|b| b.number).collect::<Vec<_>>(), [2]);
        // The tip of the first merge, the second not in: work since.
        assert!(select(&prs, &[], &[branch("b", "one", "2026-11-01T10", false)], "main", &none, None).is_empty());
    }

    #[test]
    fn remote_urls() {
        assert_eq!(owner_repo("git@github.com:Me/Repo.git").as_deref(), Some("me/repo"));
        assert_eq!(owner_repo("https://github.com/me/repo").as_deref(), Some("me/repo"));
        assert_eq!(owner_repo("ssh://git@ghe.example.com:2222/me/repo.git/").as_deref(), Some("me/repo"));
        assert_eq!(owner_repo("/tmp/origin.git"), None);
    }

    /// A local origin: one branch deleted, one gone already, one its
    /// hook refuses; the remote-tracking refs follow.
    #[cfg(unix)]
    #[test]
    fn deletes_from_a_local_origin() {
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("ideae-merged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git").current_dir(dir).args(["-c", "user.name=T", "-c", "user.email=t@example.com"]).args(args).output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        git(&root, &["init", "-q", "--bare", "origin.git"]);
        git(&root, &["init", "-q", "-b", "main", "repo"]);
        let repo = root.join("repo");
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&repo, &["remote", "add", "origin", "../origin.git"]);
        for to in ["main", "done", "gone", "kept"] {
            git(&repo, &["push", "-q", "origin", &format!("main:{to}")]);
        }
        git(&repo, &["fetch", "-q", "origin"]);
        git(&root.join("origin.git"), &["branch", "-D", "gone"]);
        std::fs::write(root.join("origin.git/hooks/update"), "#!/bin/sh\n[ \"$1\" = refs/heads/kept ] && exit 1\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(root.join("origin.git/hooks/update"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let done = delete(&repo, &["done".into(), "gone".into(), "kept".into()]);
        assert_eq!(done.deleted, ["gone", "done"]);
        assert_eq!(done.failed.iter().map(|(b, _)| b.as_str()).collect::<Vec<_>>(), ["kept"]);
        let left = git(&repo, &["for-each-ref", "--format=%(refname:strip=3)", "refs/remotes/origin"]);
        assert_eq!(left.lines().collect::<Vec<_>>(), ["kept", "main"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_push_told_branch_by_branch() {
        let stdout = "To github.com:me/repo.git\n-\t:refs/heads/a\t[deleted]\n!\t:refs/heads/b\t[remote rejected] (protected branch hook declined)\nDone\n";
        let stderr = "error: unable to delete 'c': remote ref does not exist\nerror: failed to push some refs";
        let asked = ["a", "b", "c", "d"].map(String::from);
        let done = parse_push(stdout, stderr, &asked);
        assert_eq!(done.deleted, ["a", "c"]);
        assert_eq!(done.failed[0], ("b".to_string(), "protected: protected branch hook declined".to_string()));
        assert_eq!(done.failed[1].0, "d");
    }
}
