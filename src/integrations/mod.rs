//! Trackers (Jira, Linear, GitHub and GitLab issues) and code hosts (GitHub
//! pull requests, GitLab merge requests), each set up in the layers of the
//! settings (crate::layers) as an integration: a kind, an address and the
//! keys it owns. Nothing is guessed from what was pasted before: a key's
//! site is the integration that owns its prefix.
//!
//! What has no I/O (a link read, a link made) may run anywhere; the rest
//! blocks and runs off the UI thread, as `pr.rs` always did.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

pub mod github;
pub mod gitlab;
pub mod http;
pub mod read;
pub mod secret_store;
pub mod tickets;

/// What an integration is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Jira,
    Linear,
    YouTrack,
    GitHub,
    GitLab,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Jira, Kind::Linear, Kind::YouTrack, Kind::GitHub, Kind::GitLab];

    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.word().eq_ignore_ascii_case(word.trim()))
    }

    /// As `kind` says it in the settings and `.ideae.toml`.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Jira => "jira",
            Kind::Linear => "linear",
            Kind::YouTrack => "youtrack",
            Kind::GitHub => "github",
            Kind::GitLab => "gitlab",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Jira => "Jira",
            Kind::Linear => "Linear",
            Kind::YouTrack => "YouTrack",
            Kind::GitHub => "GitHub",
            Kind::GitLab => "GitLab",
        }
    }

    /// Whether its tickets are keys like `DEV-123`, owned by prefix.
    pub fn keyed(self) -> bool {
        matches!(self, Kind::Jira | Kind::Linear | Kind::YouTrack)
    }
}

/// One integration as the layers put it together.
#[derive(Clone, Debug, PartialEq)]
pub struct Integration {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    /// Its address without a trailing slash: `https://acme.atlassian.net`,
    /// `https://corp.example.com/jira`, `https://gitlab.example.com`.
    pub base: String,
    /// Prefixes of keys (Jira, Linear, YouTrack), or repositories
    /// (`owner/repo`, `group/sub/project`) for GitHub and GitLab issues.
    pub keys: Vec<String>,
    pub from: crate::layers::Layer,
    /// Its token's account, and whether it may be read with it: you gave a
    /// token for this very address. Not connected, it asks nothing of the network.
    pub account: String,
    pub connected: bool,
    /// Its tickets may be read by a task's agent (the MCP tool `ticket`).
    pub agent_read: bool,
}

impl Integration {
    /// Out of a layered entry; none for one of a kind unknown here.
    pub fn from_entry(entry: &crate::layers::Entry, from: crate::layers::Layer) -> Option<Self> {
        let kind = Kind::parse(entry.kind.as_deref()?)?;
        let base = normal_base(entry.url.as_deref()?)?;
        let name = entry.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("{} {}", kind.label(), host_of(&base)));
        let keys = entry.keys.clone().unwrap_or_default().into_iter().map(|k| k.trim().trim_end_matches('-').to_string()).filter(|k| !k.is_empty()).collect();
        let connected = entry.auth.as_deref() == Some("token") && entry.bound.as_deref().and_then(normal_base).as_deref() == Some(base.as_str());
        Some(Self { id: entry.id.clone(), kind, name, base, keys, from, account: entry.account.clone().unwrap_or_default(), connected, agent_read: entry.agent_read == Some(true) && connected })
    }

    /// What its token is kept by: its host and account, not its layer, so
    /// two spaces of one Jira share it and two Jiras never do.
    pub fn secret_id(&self) -> String {
        secret_id(&self.base, &self.account)
    }

    pub fn host(&self) -> &str {
        host_of(&self.base)
    }

    /// Whether it owns `prefix` (`DEV`), ignoring case.
    pub fn owns(&self, prefix: &str) -> bool {
        self.keys.iter().any(|k| k.eq_ignore_ascii_case(prefix))
    }

    pub fn tracker(&self) -> Option<Box<dyn Tracker>> {
        match self.kind {
            Kind::Jira => Some(Box::new(tickets::Jira { base: self.base.clone() })),
            Kind::Linear => Some(Box::new(tickets::Linear { base: self.base.clone() })),
            Kind::YouTrack => Some(Box::new(tickets::YouTrack { base: self.base.clone() })),
            Kind::GitHub | Kind::GitLab => Some(Box::new(tickets::Issues { kind: self.kind, base: self.base.clone() })),
        }
    }

    /// GitHub and GitLab are code hosts as well as trackers.
    pub fn is_code_host(&self) -> bool {
        matches!(self.kind, Kind::GitHub | Kind::GitLab)
    }
}

/// The keyring's name of a token: host and account.
pub fn secret_id(base: &str, account: &str) -> String {
    format!("{}|{}", host_of(base), account.trim())
}

/// `https://host[:port]/path`, no trailing slash, no query; a bare host gets https.
pub fn normal_base(url: &str) -> Option<String> {
    let url = url.trim();
    let url = if url.contains("://") { url.to_string() } else { format!("https://{url}") };
    let (scheme, rest) = url.split_once("://")?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default().trim_end_matches('/');
    let host = rest.split('/').next().unwrap_or_default();
    (!host.is_empty() && matches!(scheme, "http" | "https")).then(|| format!("{scheme}://{rest}"))
}

/// The host of an address, its port kept.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or_default()
}

/// A ticket of some tracker: its key as the tracker writes it (`DEV-123`,
/// `group/project#45`) and the integration it is of, when one owns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub key: String,
    pub kind: Option<Kind>,
    pub integration: Option<String>,
    /// Its link: made by the integration, or the one pasted.
    pub url: Option<String>,
}

/// What a tracker does without asking anything of the network.
pub trait Tracker {
    /// The key of a link to one of its tickets.
    fn parse_url(&self, url: &str) -> Option<String>;
    /// The link of a ticket by its key.
    fn url_for(&self, key: &str) -> String;
}

/// Why a code host or a tracker did not answer, and what to do about it.
#[derive(Clone, Debug, PartialEq)]
pub enum IntError {
    NotSignedIn { hint: String },
    NotFound,
    Forbidden,
    RateLimited,
    ToolMissing(&'static str),
    NoRepository(String),
    Network(String),
    Parse(String),
}

impl std::fmt::Display for IntError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IntError::NotSignedIn { hint } => write!(f, "Not signed in: {hint}"),
            IntError::NotFound => write!(f, "Not found"),
            IntError::Forbidden => write!(f, "Not allowed: the account signed in has no access"),
            IntError::RateLimited => write!(f, "Too many requests for now: asked again later"),
            IntError::ToolMissing(tool) => write!(f, "{tool} is not installed: install it, or give the integration a token"),
            IntError::NoRepository(why) => write!(f, "{why}"),
            IntError::Network(why) => write!(f, "{why}"),
            IntError::Parse(why) => write!(f, "An answer not understood: {why}"),
        }
    }
}

impl IntError {
    /// Out of what a CLI (`gh`, `glab`) said on its way out.
    pub fn from_cli(tool: &'static str, login: &str, stderr: &str) -> Self {
        let said = stderr.trim();
        let low = said.to_ascii_lowercase();
        if low.contains("auth login") || low.contains("not logged") || low.contains("401") || low.contains("unauthorized") || low.contains("authentication") {
            IntError::NotSignedIn { hint: format!("run `{login}`") }
        } else if low.contains("404") || low.contains("not found") || low.contains("could not resolve") {
            IntError::NotFound
        } else if low.contains("403") || low.contains("forbidden") {
            IntError::Forbidden
        } else if low.contains("rate limit") || low.contains("429") {
            IntError::RateLimited
        } else {
            IntError::Network(format!("{tool}: {}", said.lines().last().unwrap_or(said)))
        }
    }
}

/// A request to work on in a task of its own: a colleague's, to review or go on with.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestInfo {
    pub pr: Pr,
    pub head: String,
    pub title: String,
    /// From a fork: its branch is not one this repository's remote has.
    pub cross: bool,
}

/// `(kind, number)` of a link to a request: `…/pull/12`, `…/-/merge_requests/12`.
pub fn request_of_url(url: &str) -> Option<(Kind, u64)> {
    let number = |after: &str| after.split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok());
    if let Some((_, after)) = url.split_once("/-/merge_requests/") {
        return Some((Kind::GitLab, number(after)?));
    }
    let (_, after) = url.split_once("/pull/")?;
    Some((Kind::GitHub, number(after)?))
}

/// The repository a request's link is of: `owner/repo`, `group/sub/project`.
pub fn request_repo_of_url(url: &str) -> Option<String> {
    let before = url.split_once("/-/merge_requests/").or_else(|| url.split_once("/pull/"))?.0;
    let path = before.split_once("://").map_or(before, |(_, rest)| rest).split_once('/')?.1.trim_matches('/');
    (path.contains('/')).then(|| path.to_string())
}

/// Whether a request's link is of the repository at `root` (upstream or
/// origin): else what to say. Blocking: reads its remotes.
pub fn request_is_of(root: &Path, url: &str) -> Result<(), String> {
    let Some(of) = request_repo_of_url(url) else { return Ok(()) };
    let remotes = crate::git::run(root, "git", &["remote"]).unwrap_or_default();
    let ours: Vec<String> = remotes
        .lines()
        .map(str::trim)
        .filter(|n| matches!(*n, "upstream" | "origin"))
        .filter_map(|n| crate::git::run(root, "git", &["remote", "get-url", n]).ok().and_then(|u| remote_path(&u)))
        .collect();
    if ours.is_empty() || ours.iter().any(|p| p.eq_ignore_ascii_case(&of)) {
        return Ok(());
    }
    Err(format!("The link is of {of}, and this project's repository is {}: open it in that project.", ours[0]))
}

/// A pull or merge request as a code host tells of it (crate::pr's types).
pub use crate::pr::{Feedback, Pr};
pub use crate::merged_branches::ListedPr;

/// What a code host does for a repository: pull requests (GitHub) or
/// merge requests (GitLab), their checks, review, CI logs. All of it
/// blocks: off the UI thread. `root` is a checkout of the repository, here
/// or on another machine (its origin is read there; the host is asked from
/// here, where the user signed in).
pub trait CodeHost: Send + Sync {
    /// The request of each branch, in their order, asked at once.
    fn prs_for_branches(&self, root: &Path, branches: &[String]) -> Result<Vec<Option<Pr>>, IntError>;
    /// One by its number, with its head branch.
    fn pr(&self, root: &Path, number: u64) -> Result<(Pr, String), IntError>;
    /// One to check out: its head branch, title, and whether it comes from a fork.
    fn request_info(&self, root: &Path, number: u64) -> Result<RequestInfo, IntError>;
    fn feedback(&self, root: &Path, pr: &Pr) -> Result<Feedback, IntError>;
    /// The latest failed run of the branch: its address and its log's end.
    fn failed_ci_log(&self, root: &Path, branch: &str) -> Result<(String, String), IntError>;
    /// Creates a request of `head` into `base`; its address.
    fn create_pr(&self, root: &Path, head: &str, base: &str, title: &str, body: &str) -> Result<String, IntError>;
    /// The request of `head` already there, if any: its address.
    fn existing_pr(&self, root: &Path, head: &str) -> Result<Option<String>, IntError>;
    /// Your own merged requests and everyone's open ones, for the branches
    /// left behind: a colleague's merged branch is theirs to delete.
    fn merged_and_open(&self, root: &Path) -> Result<(Vec<ListedPr>, Vec<ListedPr>), IntError>;
    /// An issue's title and body.
    fn issue(&self, root: &Path, number: u64) -> Result<(String, String), IntError>;
}

// ---------------------------------------------------------------- what applies where

/// Each project's integrations, set by the window when its layers are
/// resolved, read from any thread (the code host of a task's root).
static PROJECTS: RwLock<Option<std::collections::HashMap<PathBuf, Vec<Integration>>>> = RwLock::new(None);

pub fn set_project(project: &Path, integrations: Vec<Integration>) {
    let mut all = PROJECTS.write().unwrap_or_else(|e| e.into_inner());
    all.get_or_insert_with(Default::default).insert(project.to_path_buf(), integrations);
}

/// The integrations of the project a task's root is of.
pub fn of_root(root: &Path) -> Vec<Integration> {
    let project = crate::project_config::project_of(root);
    PROJECTS.read().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|all| all.get(&project).or_else(|| all.get(root)).cloned()).unwrap_or_default()
}

/// The integrations a project's resolved layers hold; those that do not
/// say what they are are left out (`Resolved::errors` tells why).
pub fn of_resolved(resolved: &crate::layers::Resolved) -> Vec<Integration> {
    // Tickets off: a team's file says nothing.
    let tickets = crate::state::with_settings(|s| s.tickets);
    resolved
        .integrations
        .iter()
        .filter(|(_, from)| tickets || *from != crate::layers::Layer::Team)
        .filter_map(|(entry, from)| Integration::from_entry(entry, *from))
        .collect()
}

/// The remote a code host's requests are asked about: `upstream` when
/// there is one (a fork's requests and issues live there, as gh decides
/// too), else `origin`. Read where the checkout is.
pub fn request_remote(root: &Path) -> Option<String> {
    let remotes = crate::git::run(root, "git", &["remote"]).ok()?;
    let names: Vec<&str> = remotes.lines().map(str::trim).collect();
    let name = ["upstream", "origin"].into_iter().find(|n| names.contains(n))?;
    crate::git::run(root, "git", &["remote", "get-url", name]).ok().map(|u| u.trim().to_string())
}

/// Which code host a remote's URL is of: the integrations' hosts first,
/// then the well-known ones; GitHub otherwise, as before integrations
/// (gh knows the Enterprise hosts it is signed in to).
pub fn host_kind(url: &str, integrations: &[Integration]) -> Kind {
    let host = remote_host(url);
    // Not one a team's file names alone: a repository cloned from anyone
    // must not send ideae to hosts its author chose.
    let mine = integrations.iter().filter(|i| i.from != crate::layers::Layer::Team);
    if let Some(i) = mine.into_iter().find(|i| i.is_code_host() && i.host().split(':').next() == Some(host.as_str())) {
        return i.kind;
    }
    if host == "gitlab.com" || host.starts_with("gitlab.") {
        Kind::GitLab
    } else {
        Kind::GitHub
    }
}

/// The host of a remote's URL: `git@host:o/r`, `ssh://git@host:22/o/r`, `https://host/o/r`.
pub fn remote_host(url: &str) -> String {
    let url = url.trim();
    match url.split_once("://") {
        Some((_, rest)) => {
            let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
            rest.split(['/', ':']).next().unwrap_or_default().to_string()
        }
        None => {
            let rest = url.rsplit_once('@').map_or(url, |(_, r)| r);
            // `host:path`; a path on disk has no host.
            rest.split_once(':').map(|(host, _)| host).filter(|h| !h.contains('/')).unwrap_or_default().to_string()
        }
    }
}

/// The path of a remote's URL, `.git` off: `owner/repo`, `group/sub/project`.
pub fn remote_path(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let path = match url.split_once("://") {
        Some((_, rest)) => {
            let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
            rest.split_once('/')?.1
        }
        None => {
            let rest = url.rsplit_once('@').map_or(url, |(_, r)| r);
            rest.split_once(':').filter(|(host, _)| !host.contains('/') && host.len() > 1)?.1
        }
    };
    let path = path.trim_matches('/');
    (path.contains('/') && !path.split('/').any(str::is_empty)).then(|| path.to_string())
}

/// The code host of the repository at `root`. Off the UI thread: reads
/// its remotes, there for a remote task.
pub fn code_host(root: &Path) -> Result<Box<dyn CodeHost>, IntError> {
    let url = request_remote(root).ok_or_else(|| IntError::NoRepository("no remote named origin or upstream".into()))?;
    let integrations = of_root(root);
    Ok(match host_kind(&url, &integrations) {
        Kind::GitLab => Box::new(gitlab::GitLab::for_remote(&url)?),
        _ => Box::new(github::GitHub),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_request_link_s_repository() {
        assert_eq!(request_repo_of_url("https://github.com/acme/web/pull/42").as_deref(), Some("acme/web"));
        assert_eq!(request_repo_of_url("https://gitlab.example.com/a/b/c/-/merge_requests/7#note_1").as_deref(), Some("a/b/c"));
        assert_eq!(request_repo_of_url("https://github.com/pull/42"), None);
    }

    use super::*;

    #[test]
    fn remotes_are_read_for_their_host_and_path() {
        for (url, host, path) in [
            ("git@github.com:owner/repo.git", "github.com", Some("owner/repo")),
            ("https://gitlab.example.com/group/sub/project.git", "gitlab.example.com", Some("group/sub/project")),
            ("ssh://git@gitlab.example.com:2222/group/sub/project.git/", "gitlab.example.com", Some("group/sub/project")),
            ("https://user@github.com/owner/repo", "github.com", Some("owner/repo")),
            ("/srv/git/repo", "", None),
        ] {
            assert_eq!(remote_host(url), host, "{url}");
            assert_eq!(remote_path(url).as_deref(), path, "{url}");
        }
    }

    #[test]
    fn a_remote_s_host_says_which_code_host_it_is() {
        let corp = Integration { id: "corp".into(), kind: Kind::GitLab, name: "Corp".into(), base: "https://git.corp.example.com".into(), keys: vec![], from: crate::layers::Layer::Global, account: String::new(), connected: false, agent_read: false };
        assert_eq!(host_kind("git@git.corp.example.com:a/b.git", std::slice::from_ref(&corp)), Kind::GitLab);
        assert_eq!(host_kind("git@git.corp.example.com:a/b.git", &[]), Kind::GitHub, "not set up: as before, gh");
        assert_eq!(host_kind("https://gitlab.com/a/b", &[]), Kind::GitLab);
        assert_eq!(host_kind("https://gitlab.example.com/a/b", &[]), Kind::GitLab);
        assert_eq!(host_kind("git@github.com:a/b", &[]), Kind::GitHub);
    }

    #[test]
    fn an_entry_becomes_an_integration() {
        let entry = crate::layers::Entry {
            id: "acme".into(),
            kind: Some("Jira".into()),
            url: Some("https://example.atlassian.net/".into()),
            keys: Some(vec!["DEV-".into(), " OPS".into()]),
            ..Default::default()
        };
        let i = Integration::from_entry(&entry, crate::layers::Layer::Space).unwrap();
        assert_eq!((i.kind, i.base.as_str(), i.keys.clone(), i.name.as_str()), (Kind::Jira, "https://example.atlassian.net", vec!["DEV".to_string(), "OPS".to_string()], "Jira example.atlassian.net"));
        assert!(i.owns("dev"));
        assert_eq!(normal_base("corp.example.com/jira/?x=1").as_deref(), Some("https://corp.example.com/jira"));
        assert_eq!(normal_base("ftp://x"), None);
        let unknown = crate::layers::Entry { kind: Some("trello".into()), ..entry };
        assert!(Integration::from_entry(&unknown, crate::layers::Layer::Global).is_none());
    }
}
