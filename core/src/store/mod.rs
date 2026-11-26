//! A machine's store: its projects and tasks and all of the work about them,
//! kept for every window that shows them (the window here, one over ssh).
//! What is a window's own (its layout, tabs, which projects it shows and
//! where) and a person's (spaces, how things look) is not here.
//!
//! Changed only through [`Op`]s, applied in order by whoever holds the
//! store: the store decides what must be one machine-wide (a task's id, its
//! ports' slot, a variant's number, a question's or a crew record's id, an
//! archive's key) and refuses what would break it (a path two tasks share,
//! a task another window is archiving). What a window keeps of its own
//! types (a project's settings layer, a check's run, a profile) is kept
//! here whole as JSON, its fields as the window wrote them, unknown ones too.

pub mod file;
pub mod host;
pub mod migrate;
pub mod records;
pub mod sync;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use records::{AgentSession, CrewRecord, QuestionState, ReviewNote, TaskLink};

/// The store's format: one a build does not know is not written over.
pub const VERSION: u32 = 1;

fn is_zero<T: Default + PartialEq>(n: &T) -> bool {
    *n == T::default()
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Machine {
    pub version: u32,
    /// Raised by every op applied: what a window has seen up to.
    pub rev: u64,
    /// The rev last written into the state.json kept for older builds:
    /// one written since without it is an older build's (migrate.rs).
    #[serde(skip_serializing_if = "is_zero")]
    pub mirrored: u64,
    pub projects: Vec<Project>,
    pub crew: Vec<CrewRecord>,
    /// The last id given: tasks', crew records'. Never given twice.
    pub last_task: u64,
    pub last_crew: u32,
    /// The Machine layer of the settings: what describes this machine (where
    /// worktrees go, its language servers, kept awake, its agents), by the
    /// names Global's fields have (migrate.rs, MACHINE_FIELDS).
    #[serde(skip_serializing_if = "Value::is_null")]
    pub settings: Value,
    /// Its agents' profiles, as the window writes them (each has an `id`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Project {
    /// The main checkout: its key.
    pub path: PathBuf,
    /// In their order on this machine.
    pub tasks: Vec<Task>,
    /// Its own layer of the settings (an `Overrides`).
    #[serde(skip_serializing_if = "Value::is_null")]
    pub settings: Value,
    /// Its `.ideae.toml` kept for yourself ("Only for me").
    #[serde(skip_serializing_if = "Value::is_null")]
    pub own: Value,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub trusted: BTreeMap<String, bool>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub check: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub check_auto: bool,
    /// The judge's verdict on each group of variants (a `Run`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub verdicts: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale_verdicts: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hints_off: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub made_by_agents: BTreeMap<PathBuf, PathBuf>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Task {
    /// Given by the store, never changed: its path may be (a restore).
    pub id: u64,
    /// The worktree; for the main thread, the project's own path.
    pub path: PathBuf,
    pub branch: String,
    pub title: String,
    pub main: bool,
    /// Being made (its worktree added): shown busy, gone if that fails.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    pub starred: bool,
    pub last_active: u64,
    /// Its share of the services' ports, given by the store: 0 for the main checkout.
    pub port_slot: u32,
    pub group: String,
    pub variant: u32,
    pub note: String,
    pub prompt: String,
    pub created: u64,
    pub done_at: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub seen_at: u64,
    pub base: String,
    /// How the check last run in every variant went in this one (a `Run`).
    #[serde(skip_serializing_if = "Value::is_null")]
    pub check: Value,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<AgentSession>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub archive: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub archive_keep: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fresh: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub woke: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<QuestionState>,
    #[serde(skip_serializing_if = "is_zero")]
    pub asked: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_by: Option<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<TaskLink>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub links_read: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub review: bool,
    /// What was done in it that is no tab's: review notes not sent yet,
    /// files marked viewed, bookmarks. A window's tabs are its own.
    #[serde(skip_serializing_if = "TaskWork::is_empty")]
    pub work: TaskWork,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct TaskWork {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub review: Vec<ReviewNote>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub viewed: BTreeMap<String, u64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub bookmarks: BTreeMap<String, Vec<u32>>,
}

impl TaskWork {
    pub fn is_empty(&self) -> bool {
        self.review.is_empty() && self.viewed.is_empty() && self.bookmarks.is_empty()
    }
}

/// A window, as the one that holds the store knows it (its connection).
pub type WindowId = u64;

/// What a lease holds: a task, or a deferred task's archive (no task yet).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Leased {
    Task(u64),
    Archive(String),
}

/// A change asked of the store.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "op")]
pub enum Op {
    AddProject { path: PathBuf, settings: Value },
    RemoveProject { path: PathBuf },
    /// Fields of a project by their names, its path and tasks excepted.
    SetProject { path: PathBuf, fields: Map<String, Value> },
    /// A task in `project`, after the task `after` (none: first). Its id,
    /// port slot and, in a group, its variant number are the store's.
    CreateTask { project: PathBuf, task: Task, after: Option<u64> },
    /// Fields of a task by their names; not those the store gives.
    SetTask { id: u64, fields: Map<String, Value> },
    Repath { id: u64, path: PathBuf },
    MoveTask { id: u64, after: Option<u64> },
    DeleteTask { id: u64 },
    /// Its ports' slot, given now if it has none (one made before slots).
    PortSlot { id: u64 },
    /// A question asked aside: its id is the task's next.
    NewQuestion { id: u64, question: QuestionState },
    NewCrew { record: CrewRecord },
    /// The crew records, merged by a window's sync (sync.rs).
    Crew { crew: Vec<CrewRecord> },
    /// The next crew id, given now (an agent is told it at once).
    ReserveCrew,
    /// The next question id of the task at `task`, given now.
    ReserveQuestion { task: PathBuf },
    /// A key for an archive of task `id` made at `at` (unix seconds), none
    /// of the project's archives having it.
    ArchiveKey { id: u64, taken: Vec<String>, at: u64 },
    Lease { what: Leased },
    Release { what: Leased },
    Settings { settings: Value },
    Profiles { profiles: Vec<Value> },
}

/// What an op gave back.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum Applied {
    Done,
    Task(u64),
    Slot(u32),
    Question(u32),
    Crew(u32),
    Key(String),
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum Refused {
    NoProject(PathBuf),
    NoTask(u64),
    /// Another task (or project) has it.
    Taken(String),
    /// Another window is at it (archiving, restoring).
    Leased,
    /// Not a field that op may set.
    NotSettable(String),
    Invalid(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NoProject(p) => write!(f, "no project at {}", p.display()),
            Refused::NoTask(id) => write!(f, "no task {id}"),
            Refused::Taken(what) => write!(f, "{what} is another's"),
            Refused::Leased => write!(f, "another window is at it"),
            Refused::NotSettable(field) => write!(f, "{field} is not set so"),
            Refused::Invalid(why) => write!(f, "{why}"),
        }
    }
}

/// Fields a `SetTask` may not touch: the store gives them, or they have ops of their own.
/// Its port slot, variant number and questions' count may be set: to
/// numbers no other task has (one window alone gives them, for now).
const TASK_KEPT: &[&str] = &["id", "path"];
const PROJECT_KEPT: &[&str] = &["path", "tasks"];

/// The machine and who holds what of it now.
#[derive(Default)]
pub struct Store {
    pub machine: Machine,
    leases: Vec<(Leased, WindowId)>,
}

impl Store {
    pub fn new(machine: Machine) -> Self {
        Self { machine, leases: Vec::new() }
    }

    pub fn task(&self, id: u64) -> Option<&Task> {
        self.machine.projects.iter().flat_map(|p| &p.tasks).find(|t| t.id == id)
    }

    pub fn task_at(&self, path: &Path) -> Option<&Task> {
        self.machine.projects.iter().flat_map(|p| &p.tasks).find(|t| t.path == path)
    }

    fn task_mut(&mut self, id: u64) -> Result<&mut Task, Refused> {
        self.machine.projects.iter_mut().flat_map(|p| &mut p.tasks).find(|t| t.id == id).ok_or(Refused::NoTask(id))
    }

    fn project_of(&self, id: u64) -> Result<usize, Refused> {
        self.machine.projects.iter().position(|p| p.tasks.iter().any(|t| t.id == id)).ok_or(Refused::NoTask(id))
    }

    /// Whether another window than `by` holds `what`.
    fn held_by_other(&self, what: &Leased, by: WindowId) -> bool {
        self.leases.iter().any(|(held, holder)| held == what && *holder != by)
    }

    /// What `by` held goes: its window has gone.
    pub fn release_all(&mut self, by: WindowId) {
        self.leases.retain(|(_, holder)| *holder != by);
    }

    /// Applies `op` asked by window `by`; the machine's rev rises when it changed.
    pub fn apply(&mut self, by: WindowId, op: Op) -> Result<Applied, Refused> {
        let before = self.machine.rev;
        let applied = self.apply_inner(by, op)?;
        if self.machine.rev == before {
            self.machine.rev += 1;
        }
        Ok(applied)
    }

    fn apply_inner(&mut self, by: WindowId, op: Op) -> Result<Applied, Refused> {
        match op {
            Op::AddProject { path, settings } => {
                if self.machine.projects.iter().any(|p| p.path == path) {
                    return Err(Refused::Taken(path.display().to_string()));
                }
                self.machine.projects.push(Project { path, settings, ..Default::default() });
                Ok(Applied::Done)
            }
            Op::RemoveProject { path } => {
                let at = self.machine.projects.iter().position(|p| p.path == path).ok_or(Refused::NoProject(path))?;
                if self.machine.projects[at].tasks.iter().any(|t| self.held_by_other(&Leased::Task(t.id), by)) {
                    return Err(Refused::Leased);
                }
                self.machine.projects.remove(at);
                Ok(Applied::Done)
            }
            Op::SetProject { path, fields } => {
                let project = self.machine.projects.iter_mut().find(|p| p.path == path).ok_or(Refused::NoProject(path))?;
                *project = patched(project, fields, PROJECT_KEPT)?;
                Ok(Applied::Done)
            }
            Op::CreateTask { project, mut task, after } => {
                let pi = self.machine.projects.iter().position(|p| p.path == project).ok_or(Refused::NoProject(project))?;
                if self.task_at(&task.path).is_some() {
                    return Err(Refused::Taken(task.path.display().to_string()));
                }
                // A branch is git's to keep to one worktree (two detached ones may show the same commit).
                let tasks = &self.machine.projects[pi].tasks;
                // Given numbers are kept when no other task has them; else the store's.
                if !task.group.is_empty() && (task.variant == 0 || tasks.iter().any(|t| t.group == task.group && t.variant == task.variant)) {
                    task.variant = tasks.iter().filter(|t| t.group == task.group).map(|t| t.variant).max().unwrap_or(0) + 1;
                }
                if task.main {
                    task.port_slot = 0;
                } else if task.port_slot == 0 || tasks.iter().any(|t| t.port_slot == task.port_slot) {
                    task.port_slot = free_slot(tasks);
                }
                self.machine.last_task += 1;
                task.id = self.machine.last_task;
                let id = task.id;
                let tasks = &mut self.machine.projects[pi].tasks;
                let at = match after {
                    None => 0,
                    Some(after) => tasks.iter().position(|t| t.id == after).map_or(tasks.len(), |i| i + 1),
                };
                tasks.insert(at, task);
                Ok(Applied::Task(id))
            }
            Op::SetTask { id, fields } => {
                let pi = self.project_of(id)?;
                let task = self.machine.projects[pi].tasks.iter().find(|t| t.id == id).ok_or(Refused::NoTask(id))?;
                let set = patched(task, fields, TASK_KEPT)?;
                let others = || self.machine.projects[pi].tasks.iter().filter(|t| t.id != id);
                if set.port_slot != task.port_slot && set.port_slot > 0 && others().any(|t| t.port_slot == set.port_slot) {
                    return Err(Refused::Taken(format!("port slot {}", set.port_slot)));
                }
                if set.variant != task.variant && !set.group.is_empty() && others().any(|t| t.group == set.group && t.variant == set.variant) {
                    return Err(Refused::Taken(format!("variant {} of {}", set.variant, set.group)));
                }
                *self.task_mut(id)? = set;
                Ok(Applied::Done)
            }
            Op::Repath { id, path } => {
                if self.task_at(&path).is_some_and(|t| t.id != id) {
                    return Err(Refused::Taken(path.display().to_string()));
                }
                self.task_mut(id)?.path = path;
                Ok(Applied::Done)
            }
            Op::MoveTask { id, after } => {
                let pi = self.project_of(id)?;
                let tasks = &mut self.machine.projects[pi].tasks;
                let from = tasks.iter().position(|t| t.id == id).ok_or(Refused::NoTask(id))?;
                let task = tasks.remove(from);
                let at = match after {
                    None => 0,
                    Some(after) => tasks.iter().position(|t| t.id == after).map_or(tasks.len(), |i| i + 1),
                };
                tasks.insert(at, task);
                Ok(Applied::Done)
            }
            Op::DeleteTask { id } => {
                if self.held_by_other(&Leased::Task(id), by) {
                    return Err(Refused::Leased);
                }
                let pi = self.project_of(id)?;
                let project = &mut self.machine.projects[pi];
                let path = project.tasks.iter().find(|t| t.id == id).map(|t| t.path.clone()).unwrap_or_default();
                project.tasks.retain(|t| t.id != id);
                // Worktrees its agent made are no task's now.
                project.made_by_agents.retain(|_, of| *of != path);
                self.leases.retain(|(held, _)| *held != Leased::Task(id));
                Ok(Applied::Done)
            }
            Op::PortSlot { id } => {
                let pi = self.project_of(id)?;
                let tasks = &self.machine.projects[pi].tasks;
                let task = tasks.iter().find(|t| t.id == id).ok_or(Refused::NoTask(id))?;
                if task.main || task.port_slot > 0 {
                    return Ok(Applied::Slot(task.port_slot));
                }
                let slot = free_slot(tasks);
                self.task_mut(id)?.port_slot = slot;
                Ok(Applied::Slot(slot))
            }
            Op::NewQuestion { id, mut question } => {
                let task = self.task_mut(id)?;
                task.asked += 1;
                question.id = task.asked;
                task.questions.push(question);
                Ok(Applied::Question(task.asked))
            }
            Op::NewCrew { mut record } => {
                self.machine.last_crew = self.machine.last_crew.max(self.machine.crew.iter().map(|c| c.id).max().unwrap_or(0)) + 1;
                record.id = self.machine.last_crew;
                self.machine.crew.push(record);
                // The latest hundred are kept, as before.
                let over = self.machine.crew.len().saturating_sub(100);
                self.machine.crew.drain(..over);
                Ok(Applied::Crew(self.machine.last_crew))
            }
            Op::ReserveCrew => {
                self.machine.last_crew = self.machine.last_crew.max(self.machine.crew.iter().map(|c| c.id).max().unwrap_or(0)) + 1;
                Ok(Applied::Crew(self.machine.last_crew))
            }
            Op::ReserveQuestion { task } => {
                let id = self.task_at(&task).map(|t| t.id).ok_or_else(|| Refused::Invalid(format!("no task at {}", task.display())))?;
                let task = self.task_mut(id)?;
                task.asked = task.asked.max(task.questions.iter().map(|q| q.id).max().unwrap_or(0)) + 1;
                Ok(Applied::Question(task.asked))
            }
            Op::Crew { crew } => {
                self.machine.last_crew = self.machine.last_crew.max(crew.iter().map(|c| c.id).max().unwrap_or(0));
                self.machine.crew = crew;
                Ok(Applied::Done)
            }
            Op::ArchiveKey { id, taken, at } => {
                let task = self.task(id).ok_or(Refused::NoTask(id))?;
                let stem = format!("{}-{at}", task.branch.replace('/', "-"));
                let key = (1..).map(|n| if n == 1 { stem.clone() } else { format!("{stem}-{n}") }).find(|k| !taken.contains(k)).unwrap_or(stem);
                Ok(Applied::Key(key))
            }
            Op::Lease { what } => {
                if self.held_by_other(&what, by) {
                    return Err(Refused::Leased);
                }
                if !self.leases.iter().any(|(held, holder)| *held == what && *holder == by) {
                    self.leases.push((what, by));
                }
                Ok(Applied::Done)
            }
            Op::Release { what } => {
                self.leases.retain(|(held, holder)| !(*held == what && *holder == by));
                Ok(Applied::Done)
            }
            Op::Settings { settings } => {
                self.machine.settings = settings;
                Ok(Applied::Done)
            }
            Op::Profiles { profiles } => {
                self.machine.profiles = profiles;
                Ok(Applied::Done)
            }
        }
    }
}

/// The lowest slot none of `tasks` has: 0 is the main checkout's.
fn free_slot(tasks: &[Task]) -> u32 {
    (1..).find(|s| !tasks.iter().any(|t| t.port_slot == *s)).unwrap_or(1)
}

/// `value` with `fields` set over its own, none of `kept` among them; a
/// field set to null is its default again.
fn patched<T: Serialize + for<'de> Deserialize<'de>>(value: &T, fields: Map<String, Value>, kept: &[&str]) -> Result<T, Refused> {
    if let Some(field) = fields.keys().find(|k| kept.contains(&k.as_str())) {
        return Err(Refused::NotSettable(field.clone()));
    }
    let mut json = serde_json::to_value(value).map_err(|e| Refused::Invalid(e.to_string()))?;
    let Value::Object(object) = &mut json else { return Err(Refused::Invalid("not an object".into())) };
    for (k, v) in fields {
        match v {
            Value::Null => {
                object.remove(&k);
            }
            v => {
                object.insert(k, v);
            }
        }
    }
    serde_json::from_value(json).map_err(|e| Refused::Invalid(e.to_string()))
}

#[cfg(test)]
mod tests;
