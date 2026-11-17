//! Which agent a task runs: profiles (a program, its flags and its
//! environment: two accounts of one agent are two profiles) and adapters,
//! what each kind of agent can do. Every capability is optional; the app
//! does without what an adapter cannot tell.
//!
//! Agents stay programs in a terminal: an adapter only says how to start
//! and resume them, how they report, where their sessions are kept and how
//! to ask them one question.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub mod claude;
pub mod codex;
pub mod find;
pub mod generic;
pub mod opencode;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Claude,
    Codex,
    OpenCode,
    Generic,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Claude, Kind::Codex, Kind::OpenCode, Kind::Generic];

    /// By the program a command starts: `claude`, a path to `codex`…
    pub fn detect(command: &str) -> Kind {
        let Some(program) = command.split_whitespace().next() else { return Kind::Generic };
        let name = Path::new(program).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        match name.trim_end_matches(".exe").trim_end_matches(".cmd").trim_end_matches(".ps1") {
            "claude" => Kind::Claude,
            "codex" => Kind::Codex,
            "opencode" => Kind::OpenCode,
            _ => Kind::Generic,
        }
    }

    pub fn adapter(self) -> &'static dyn Adapter {
        match self {
            Kind::Claude => &claude::Claude,
            Kind::Codex => &codex::Codex,
            Kind::OpenCode => &opencode::OpenCode,
            Kind::Generic => &generic::Generic,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Claude => "Claude Code",
            Kind::Codex => "Codex",
            Kind::OpenCode => "OpenCode",
            Kind::Generic => "Any other program",
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
            Kind::OpenCode => "opencode",
            Kind::Generic => "",
        }
    }
}

/// An agent as the user set it up.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Profile {
    /// Stable: tasks and projects name their agent by it.
    pub id: String,
    pub name: String,
    pub kind: Kind,
    /// The program and its flags, as typed in a shell: `claude --model opus`.
    pub command: String,
    /// Set for the agent only: `CLAUDE_CONFIG_DIR`, `CODEX_HOME` for a
    /// second account.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

pub const DEFAULT_ID: &str = "claude";

impl Profile {
    /// What a fresh install has, and what a Claude command of an older
    /// state becomes.
    pub fn claude_code() -> Self {
        Self { id: DEFAULT_ID.into(), name: "Claude Code".into(), kind: Kind::Claude, command: "claude".into(), env: BTreeMap::new() }
    }

    /// The single agent command of an older state, as a profile.
    pub fn from_command(command: &str) -> Self {
        let command = command.trim().to_string();
        let kind = Kind::detect(&command);
        if kind == Kind::Claude {
            return Self { command, ..Self::claude_code() };
        }
        let name = match kind {
            Kind::Generic => command.split_whitespace().next().map(|p| Path::new(p).file_name().map_or(p.to_string(), |n| n.to_string_lossy().into_owned())).unwrap_or_default(),
            kind => kind.label().to_string(),
        };
        Self { id: "agent".into(), name, kind, command, env: BTreeMap::new() }
    }

    pub fn adapter(&self) -> &'static dyn Adapter {
        self.kind.adapter()
    }

    /// The mark of the agent on a task: C, X, O, or the name's first letter.
    pub fn letter(&self) -> String {
        match self.kind {
            Kind::Claude if self.name.is_empty() || self.name.starts_with("Claude") => "C".into(),
            Kind::Codex if self.name.is_empty() || self.name.starts_with("Codex") => "X".into(),
            Kind::OpenCode if self.name.is_empty() || self.name.starts_with("OpenCode") => "O".into(),
            _ => self.name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into()),
        }
    }

    /// Its environment, `~/` made the home folder.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
        self.env
            .iter()
            .filter(|(key, _)| !key.trim().is_empty())
            .map(|(key, value)| {
                let value = match value.strip_prefix("~/") {
                    Some(rest) if !home.is_empty() => format!("{home}/{rest}"),
                    _ => value.clone(),
                };
                (key.trim().to_string(), value)
            })
            .collect()
    }

    /// A variable as the agent sees it: its own, else the app's.
    pub fn var(&self, name: &str) -> Option<String> {
        self.env_vars().into_iter().find(|(key, _)| key == name).map(|(_, value)| value).or_else(|| std::env::var(name).ok()).filter(|v| !v.is_empty())
    }

    /// `KEY=value` lines, for editing.
    pub fn env_text(&self) -> String {
        self.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n")
    }

    pub fn parse_env(text: &str) -> BTreeMap<String, String> {
        text.lines()
            .filter_map(|line| {
                let line = line.trim();
                let line = line.strip_prefix("export ").unwrap_or(line);
                let (key, value) = line.split_once('=')?;
                let key = key.trim();
                (!key.is_empty() && !key.starts_with('#')).then(|| (key.to_string(), value.trim().trim_matches('"').to_string()))
            })
            .collect()
    }
}

/// A new id for a profile: not one of `taken`.
pub fn new_profile_id(name: &str, taken: &[Profile]) -> String {
    let stem: String = name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let stem = stem.trim_matches('-').to_string();
    let stem = if stem.is_empty() { "agent".to_string() } else { stem };
    std::iter::once(stem.clone()).chain((2..).map(|n| format!("{stem}-{n}"))).find(|id| !taken.iter().any(|p| &p.id == id)).unwrap_or(stem)
}

/// The agent of a task: the one it runs now (its last session's), else
/// the one chosen when it was made, else its project's, else the default.
/// A profile since removed counts as none.
pub fn resolve<'a>(profiles: &'a [Profile], task: &[&str], project: Option<&str>, default: &str) -> Option<&'a Profile> {
    let find = |id: &str| profiles.iter().find(|p| p.id == id);
    task.iter().filter(|id| !id.is_empty()).find_map(|id| find(id)).or_else(|| project.and_then(find)).or_else(|| find(default)).or_else(|| profiles.first())
}

/// How a session of the agent begins.
#[derive(Clone, Debug, PartialEq)]
pub enum Start {
    /// A new one: with the id ideae chose, when the agent takes one.
    New(Option<String>),
    /// Exactly this one again.
    Resume(String),
    /// The latest one in the folder, else a new one: a task of before
    /// session ids were kept.
    Last,
}

/// How an agent tells what it does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Events {
    /// When it starts working, asks, finishes: Claude Code's hooks.
    All,
    /// Only when a turn ends; that it works is guessed from its output.
    TurnEnd,
    /// Nothing: its status is guessed from its terminal.
    None,
}

/// What the app passes to the agents it starts so they can report.
#[derive(Clone, Debug, Default)]
pub struct Hooks {
    /// The `--settings` file with Claude Code's hooks.
    pub claude_settings: Option<PathBuf>,
    /// This program, run as `ideae --hook …` by an agent.
    pub exe: Option<PathBuf>,
    /// The plugin file written for OpenCode.
    pub opencode_plugin: Option<PathBuf>,
}

static HOOKS: std::sync::RwLock<Option<Hooks>> = std::sync::RwLock::new(None);

pub fn set_hooks(hooks: Hooks) {
    *HOOKS.write().unwrap_or_else(|e| e.into_inner()) = Some(hooks);
}

pub fn hooks() -> Option<Hooks> {
    HOOKS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// One question, answered once, for the app.
#[derive(Clone, Debug)]
pub enum Ask {
    /// Quick, cheap and with no tools: a commit message.
    Quick,
    /// May read these folders, nothing else: the variants' judge.
    Read(Vec<PathBuf>),
}

/// How the answer to a one-shot question comes back.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// What it prints.
    Printed,
    /// Claude Code's `--output-format json`: the answer and its tokens.
    ClaudeJson,
}

/// A one-shot run: the question goes to its standard input.
#[derive(Clone, Debug)]
pub struct OneShot {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub answer: Answer,
}

impl OneShot {
    /// As one shell line, for runs that are a process group.
    pub fn line(&self) -> String {
        std::iter::once(&self.program).chain(&self.args).map(|word| quote(word)).collect::<Vec<_>>().join(" ")
    }

    pub fn command(&self) -> std::process::Command {
        let mut command = crate::procs::command(&self.program);
        command.args(&self.args).envs(self.env.iter().map(|(k, v)| (k, v))).env_remove(crate::agent::SOCK_ENV);
        command
    }

    /// The answer, and the tokens it took when told.
    pub fn read_answer(&self, printed: &str) -> (String, u64) {
        match &self.answer {
            Answer::Printed => (printed.trim().to_string(), 0),
            Answer::ClaudeJson => claude::json_answer(printed),
        }
    }
}

/// What each kind of agent can do; what it cannot, it leaves to the defaults.
pub trait Adapter: Sync {
    /// Nobody here has run it for real: its adapter follows its source.
    fn tried_live(&self) -> bool {
        true
    }

    /// It takes a session id the app chooses when it starts.
    fn chooses_session_id(&self) -> bool {
        false
    }

    /// The shell command for the agent's terminal. `unasked`: it acts without
    /// asking, as far as it can safely (the variants of a task).
    fn command(&self, profile: &Profile, start: &Start, unasked: bool, hooks: Option<&Hooks>) -> String;

    /// Variables for the agent's terminal besides the profile's own.
    fn env(&self, _profile: &Profile, _hooks: Option<&Hooks>) -> Vec<(String, String)> {
        Vec::new()
    }

    fn events(&self) -> Events {
        Events::None
    }

    /// It says when its input is up (Claude Code's SessionStart); others
    /// get text once their terminal has been quiet a moment.
    fn reports_ready(&self) -> bool {
        false
    }

    /// It can say what a session used.
    fn tells_usage(&self) -> bool {
        false
    }

    /// The file a session is kept in: given its id, or the latest in `root`.
    fn transcript(&self, _profile: &Profile, _root: &Path, _session: Option<&str>) -> Option<PathBuf> {
        None
    }

    /// What a session has used, from its file. Not for the UI thread.
    fn usage(&self, _transcript: &Path) -> Option<crate::usage::Usage> {
        None
    }

    /// Its latest answer, from the session's file.
    fn last_answer(&self, _transcript: &Path) -> Option<String> {
        None
    }

    /// It goes on with a session of before, by its id.
    fn resumes(&self) -> bool {
        false
    }

    /// Where the files of its sessions in `root` are kept: what
    /// `session_files` are relative to. An archived session is put back
    /// under this for the worktree it is restored in.
    fn session_base(&self, _profile: &Profile, _root: &Path) -> Option<PathBuf> {
        None
    }

    /// The files of a session, for an archive: with its id when the agent
    /// did not report it (a task of before ids were kept). Not for the UI thread.
    fn session_files(&self, _profile: &Profile, _root: &Path, _session: Option<&str>) -> Option<SessionFiles> {
        None
    }

    /// A question answered once and the program gone.
    fn one_shot(&self, _profile: &Profile, _ask: &Ask) -> Option<OneShot> {
        None
    }
}

/// The files a session is kept in, relative to `base`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionFiles {
    pub base: PathBuf,
    pub id: Option<String>,
    pub files: Vec<PathBuf>,
}

/// Every file under `dir`, relative to `base`.
pub fn files_under(base: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(at) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() {
                if let Ok(rel) = entry.path().strip_prefix(base) {
                    found.push(rel.to_path_buf());
                }
            }
        }
    }
    found.sort();
    found
}

/// The program of a command, its first word.
pub fn program(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or_default()
}

/// The value after `flag` in a command (`--model opus`, `--model=opus`).
pub fn flag_value<'a>(command: &'a str, flags: &[&str]) -> Option<&'a str> {
    let words: Vec<&str> = command.split_whitespace().collect();
    words.iter().enumerate().skip(1).find_map(|(at, word)| {
        flags.iter().find_map(|flag| {
            if word == flag {
                words.get(at + 1).copied()
            } else {
                word.strip_prefix(flag).and_then(|rest| rest.strip_prefix('='))
            }
        })
    })
}

/// Whether the command has any of these words after its program.
pub fn has_flag(command: &str, flags: &[&str]) -> bool {
    command.split_whitespace().skip(1).any(|word| flags.iter().any(|flag| word == *flag || word.strip_prefix(flag).is_some_and(|rest| rest.starts_with('='))))
}

/// Single quotes, for sh and PowerShell alike; each escapes a quote its own way.
pub fn quote(s: &str) -> String {
    if cfg!(windows) { format!("'{}'", s.replace('\'', "''")) } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// `first`, and when it fails `second`. PowerShell 5.1 has no `||`.
pub fn or_else(first: &str, second: &str) -> String {
    if cfg!(windows) { format!("{first}; if (-not $?) {{ {second} }}") } else { format!("{first} || {second}") }
}

/// A random version-4 UUID: what Claude Code takes for a session id.
pub fn uuid() -> String {
    use std::hash::{BuildHasher as _, Hasher as _};
    let random = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.finish()
    };
    let (a, b) = (random(), random());
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_le_bytes());
    bytes[8..].copy_from_slice(&b.to_le_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// What starts an agent in a task's terminal.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub profile: Profile,
    pub start: Start,
    pub unasked: bool,
}

impl Launch {
    pub fn command(&self) -> String {
        self.profile.adapter().command(&self.profile, &self.start, self.unasked, hooks().as_ref())
    }

    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = self.profile.adapter().env(&self.profile, hooks().as_ref());
        env.extend(self.profile.env_vars());
        env
    }

    /// The same session once more (Restart): what was new is resumed.
    pub fn again(&self) -> Launch {
        let start = match &self.start {
            Start::New(Some(id)) | Start::Resume(id) => Start::Resume(id.clone()),
            Start::New(None) | Start::Last => Start::Last,
        };
        Launch { start, ..self.clone() }
    }

    pub fn session_id(&self) -> Option<String> {
        match &self.start {
            Start::New(id) => id.clone(),
            Start::Resume(id) => Some(id.clone()),
            Start::Last => None,
        }
    }

    /// The agent reported the session it runs: later starts resume it.
    pub fn learned(&mut self, id: &str) {
        self.start = Start::Resume(id.to_string());
    }
}

/// A task's agent session, enough to find its file off the UI thread.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRef {
    pub profile: Profile,
    pub id: Option<String>,
}

impl SessionRef {
    pub fn transcript(&self, root: &Path) -> Option<PathBuf> {
        self.profile.adapter().transcript(&self.profile, root, self.id.as_deref())
    }

    /// Its file and what it used. Not for the UI thread.
    pub fn usage(&self, root: &Path) -> Option<(PathBuf, crate::usage::Usage)> {
        let file = self.transcript(root)?;
        let usage = self.profile.adapter().usage(&file)?;
        Some((file, usage))
    }

    pub fn last_answer(&self, root: &Path) -> Option<String> {
        self.profile.adapter().last_answer(&self.transcript(root)?)
    }
}

/// The start of the task's latest session: resumed by its id, or the
/// latest in its folder when the id is not known.
pub fn start_of(session: Option<&crate::state::AgentSession>) -> Start {
    match session.and_then(|s| s.id.clone()) {
        Some(id) => Start::Resume(id),
        None => Start::Last,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, kind: Kind, command: &str) -> Profile {
        Profile { id: id.into(), name: id.into(), kind, command: command.into(), env: BTreeMap::new() }
    }

    #[test]
    fn kinds_by_program() {
        assert_eq!(Kind::detect("claude --model opus"), Kind::Claude);
        assert_eq!(Kind::detect("/usr/local/bin/codex"), Kind::Codex);
        assert_eq!(Kind::detect("opencode"), Kind::OpenCode);
        assert_eq!(Kind::detect("aider --yes"), Kind::Generic);
        assert_eq!(Kind::detect(""), Kind::Generic);
    }

    #[test]
    fn an_older_command_becomes_a_profile() {
        assert_eq!(Profile::from_command("claude"), Profile::claude_code());
        let opus = Profile::from_command(" claude --model opus ");
        assert_eq!((opus.id.as_str(), opus.name.as_str(), opus.command.as_str()), ("claude", "Claude Code", "claude --model opus"));
        let aider = Profile::from_command("/opt/bin/aider --yes");
        assert_eq!((aider.kind, aider.name.as_str(), aider.letter().as_str()), (Kind::Generic, "aider", "A"));
    }

    #[test]
    fn a_task_runs_its_own_then_its_projects_then_the_default() {
        let all = [profile("claude", Kind::Claude, "claude"), profile("codex", Kind::Codex, "codex"), profile("work", Kind::Claude, "claude")];
        let id = |p: Option<&Profile>| p.map(|p| p.id.clone());
        assert_eq!(id(resolve(&all, &[], None, "claude")).as_deref(), Some("claude"));
        assert_eq!(id(resolve(&all, &[], Some("codex"), "claude")).as_deref(), Some("codex"));
        assert_eq!(id(resolve(&all, &["work"], Some("codex"), "claude")).as_deref(), Some("work"));
        assert_eq!(id(resolve(&all, &["codex", "work"], None, "claude")).as_deref(), Some("codex"), "the session it runs, over the one it was made with");
        assert_eq!(id(resolve(&all, &["gone"], Some("gone too"), "codex")).as_deref(), Some("codex"), "what was removed counts as none");
        assert_eq!(id(resolve(&all, &[], None, "gone")).as_deref(), Some("claude"));
        assert!(resolve(&[], &[], None, "claude").is_none());
    }

    #[test]
    fn letters_and_env() {
        let mut work = profile("w", Kind::Claude, "claude");
        work.name = "Work account".into();
        assert_eq!(work.letter(), "W");
        assert_eq!(profile("c", Kind::Claude, "claude").letter(), "c".to_uppercase());
        let env = Profile::parse_env("CLAUDE_CONFIG_DIR=~/.claude-work\n# a note\nexport CODEX_HOME=\"/x/y\"\n\nbroken");
        assert_eq!(env.get("CODEX_HOME").map(String::as_str), Some("/x/y"));
        work.env = env;
        let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap();
        assert_eq!(work.var("CLAUDE_CONFIG_DIR"), Some(format!("{home}/.claude-work")));
        assert_eq!(work.env.len(), 2);
    }

    #[test]
    fn ids_are_new() {
        let all = [profile("codex", Kind::Codex, "codex")];
        assert_eq!(new_profile_id("Codex", &all), "codex-2");
        assert_eq!(new_profile_id("Claude — work", &all), "claude---work");
        let id = uuid();
        assert_eq!((id.len(), &id[14..15]), (36, "4"));
        assert_ne!(uuid(), uuid());
    }

    #[test]
    fn restart_resumes() {
        let launch = Launch { profile: Profile::claude_code(), start: Start::New(Some("abc".into())), unasked: false };
        assert_eq!(launch.again().start, Start::Resume("abc".into()));
        let legacy = Launch { start: Start::Last, ..launch.clone() };
        assert_eq!(legacy.again().start, Start::Last);
        assert_eq!(flag_value("claude --model opus", &["--model"]), Some("opus"));
        assert_eq!(flag_value("codex -m=o3", &["-m", "--model"]), Some("o3"));
        assert!(has_flag("claude --resume=x", &["--resume"]) && !has_flag("claude", &["--resume"]));
    }
}
