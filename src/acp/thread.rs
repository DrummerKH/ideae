//! A conversation with an ACP agent as the app keeps it: what the host
//! made of the agent's updates, and the window's copy of it. The host sends
//! the changes as [`Op`]s; both sides apply them with [`Thread::apply`], and
//! the journal on disk is the same ops, so a thread comes back without the
//! agent replaying it.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Thread {
    /// The agent's session id, once it has one.
    pub session: Option<String>,
    /// The agent's name and version, as it introduced itself.
    pub agent: Option<String>,
    /// What the agent calls the conversation.
    pub title: Option<String>,
    pub entries: Vec<Entry>,
    pub status: Status,
    /// Model, mode, effort and whatever else the agent lets be chosen.
    pub options: Vec<ConfigOption>,
    /// Its slash commands.
    pub commands: Vec<SlashCommand>,
    pub usage: Option<Usage>,
    /// How to sign in, while it waits for that.
    pub auth: Vec<AuthMethod>,
    /// Prompts sent while it worked, sent one by one when it is done.
    pub queued: Vec<String>,
    /// Who answers its permission questions ([`super::policy::Policy`]).
    pub policy: String,
    /// It takes images in a prompt.
    pub images: bool,
    /// The files as they were before a prompt (or before they were put
    /// back, for a notice of that): a git tree, by the entry's index.
    /// Kept as pairs: a map's number keys are text in JSON, which serde
    /// does not read back inside a tagged op.
    #[serde(with = "pairs")]
    pub checkpoints: std::collections::BTreeMap<usize, String>,
}

mod pairs {
    use serde::{Deserialize as _, Deserializer, Serialize as _, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(map: &BTreeMap<usize, String>, s: S) -> Result<S::Ok, S::Error> {
        map.iter().collect::<Vec<_>>().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<usize, String>, D::Error> {
        Ok(Vec::<(usize, String)>::deserialize(d)?.into_iter().collect())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub enum Status {
    /// The agent is being started, or its session opened.
    #[default]
    Starting,
    /// Waiting for a first prompt.
    Idle,
    Working,
    /// A tool call waits for permission.
    Asking,
    /// Its turn ended.
    Done,
    /// It wants to be signed in first.
    SignIn,
    /// The program ended: with what it said last, if anything.
    Exited(Option<String>),
}

impl Status {
    pub fn live(&self) -> bool {
        !matches!(self, Status::Exited(_))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t")]
pub enum Entry {
    User { text: String },
    Agent { text: String },
    Thought { text: String },
    Tool(Tool),
    Plan { items: Vec<PlanItem> },
    /// What the app has to say: an error, a decision taken for the user.
    Notice { text: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Tool {
    pub id: String,
    pub title: String,
    /// read, edit, delete, move, search, execute, think, fetch, other.
    pub kind: String,
    /// pending, in_progress, completed, failed.
    pub status: String,
    pub locations: Vec<Location>,
    pub diffs: Vec<Diff>,
    /// Text it gave back, its end kept when long.
    pub output: String,
    /// While it waits for an answer.
    pub permission: Option<Permission>,
    /// How the question was answered, and by whom.
    pub decided: Option<String>,
    /// The command it runs through the app (`terminal/*`), shown live.
    pub terminal: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Location {
    pub path: String,
    pub line: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Diff {
    pub path: String,
    /// None: a new file.
    pub old: Option<String>,
    pub new: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Permission {
    /// The host's number for the question, to answer it by.
    pub request: u64,
    pub options: Vec<PermissionOption>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// allow_once, allow_always, reject_once, reject_always.
    pub kind: String,
}

impl PermissionOption {
    pub fn allows(&self) -> bool {
        self.kind.starts_with("allow")
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlanItem {
    pub text: String,
    /// pending, in_progress, completed.
    pub status: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    /// model, mode, thought_level, or another the agent made up.
    pub category: String,
    pub current: String,
    pub values: Vec<OptionValue>,
    /// An older agent's modes or models (`session/set_mode`,
    /// `session/set_model`), not a config option of its own.
    pub legacy: Option<String>,
}

impl ConfigOption {
    pub fn current_name(&self) -> &str {
        self.values.iter().find(|v| v.value == self.current).map_or(self.current.as_str(), |v| v.name.as_str())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct OptionValue {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Usage {
    /// Tokens in its context now, and how many fit.
    pub used: u64,
    pub size: u64,
    /// What the session cost, as the agent says.
    pub cost: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Run in a terminal: the agent's program with these arguments.
    pub terminal: Option<Vec<String>>,
}

/// A change to a thread.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op")]
pub enum Op {
    Reset { thread: Box<Thread> },
    Push { entry: Entry },
    /// More text for the message, thought or prompt at `ix`.
    Append { ix: usize, text: String },
    Set { ix: usize, entry: Entry },
    Status { status: Status },
    Session { id: String },
    Agent { name: String },
    Title { title: String },
    Options { options: Vec<ConfigOption> },
    Choose { id: String, value: String },
    Commands { commands: Vec<SlashCommand> },
    Usage { usage: Usage },
    Auth { methods: Vec<AuthMethod> },
    Queued { prompts: Vec<String> },
    Policy { policy: String },
    Images { yes: bool },
    Checkpoint { ix: usize, tree: String },
}

impl Thread {
    pub fn apply(&mut self, op: Op) {
        match op {
            Op::Reset { thread } => *self = *thread,
            Op::Push { entry } => self.entries.push(entry),
            Op::Append { ix, text } => {
                if let Some(Entry::User { text: t } | Entry::Agent { text: t } | Entry::Thought { text: t }) = self.entries.get_mut(ix) {
                    t.push_str(&text);
                }
            }
            Op::Set { ix, entry } => {
                if let Some(at) = self.entries.get_mut(ix) {
                    *at = entry;
                }
            }
            Op::Status { status } => self.status = status,
            Op::Session { id } => self.session = Some(id),
            Op::Agent { name } => self.agent = Some(name),
            Op::Title { title } => self.title = Some(title),
            Op::Options { options } => self.options = options,
            Op::Choose { id, value } => {
                if let Some(option) = self.options.iter_mut().find(|o| o.id == id) {
                    option.current = value;
                }
            }
            Op::Commands { commands } => self.commands = commands,
            Op::Usage { usage } => self.usage = Some(usage),
            Op::Auth { methods } => self.auth = methods,
            Op::Queued { prompts } => self.queued = prompts,
            Op::Policy { policy } => self.policy = policy,
            Op::Images { yes } => self.images = yes,
            Op::Checkpoint { ix, tree } => {
                self.checkpoints.insert(ix, tree);
            }
        }
    }

    /// What the agent said last in its last turn: for notifications and
    /// for the next agent told where the work stands.
    pub fn last_said(&self) -> Option<&str> {
        let last_prompt = self.entries.iter().rposition(|e| matches!(e, Entry::User { .. })).unwrap_or(0);
        self.entries[last_prompt..].iter().rev().find_map(|e| match e {
            Entry::Agent { text } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
    }

    /// The tool call that waits for an answer.
    pub fn asking(&self) -> Option<&Tool> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::Tool(tool) if tool.permission.is_some() => Some(tool),
            _ => None,
        })
    }

    pub fn option(&self, category: &str) -> Option<&ConfigOption> {
        self.options.iter().find(|o| o.category == category)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_make_the_same_thread_on_both_sides() {
        let ops = vec![
            Op::Session { id: "s1".into() },
            Op::Push { entry: Entry::User { text: "fix it".into() } },
            Op::Status { status: Status::Working },
            Op::Push { entry: Entry::Agent { text: "Look".into() } },
            Op::Append { ix: 1, text: "ing.".into() },
            Op::Push { entry: Entry::Tool(Tool { id: "t".into(), title: "Edit a.rs".into(), status: "pending".into(), ..Default::default() }) },
            Op::Set { ix: 2, entry: Entry::Tool(Tool { id: "t".into(), title: "Edit a.rs".into(), status: "completed".into(), ..Default::default() }) },
            Op::Push { entry: Entry::Agent { text: "Done.".into() } },
            Op::Status { status: Status::Done },
        ];
        let (mut a, mut b) = (Thread::default(), Thread::default());
        for op in ops {
            let wire = serde_json::to_string(&op).unwrap();
            a.apply(op);
            b.apply(serde_json::from_str(&wire).unwrap());
        }
        assert_eq!(a, b);
        assert_eq!(a.last_said(), Some("Done."));
        assert!(matches!(&a.entries[1], Entry::Agent { text } if text == "Looking."));
        let copy = serde_json::to_string(&Op::Reset { thread: Box::new(a.clone()) }).unwrap();
        let mut c = Thread::default();
        c.apply(serde_json::from_str(&copy).unwrap());
        assert_eq!(c, a);
        // Checkpoints too, in a snapshot as in an op.
        a.apply(Op::Checkpoint { ix: 0, tree: "abc".into() });
        let copy = serde_json::to_string(&Op::Reset { thread: Box::new(a.clone()) }).unwrap();
        let mut d = Thread::default();
        d.apply(serde_json::from_str(&copy).unwrap());
        assert_eq!(d.checkpoints.get(&0).map(String::as_str), Some("abc"));
    }
}
