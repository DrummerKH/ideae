//! Who keeps a machine's store: the daemon, for every window attached to
//! it; a window itself where there is none (Windows, IDEAE_NO_DAEMON). One
//! process at a time (the lock). Opened with the state.json of the window
//! that asks first: the store is made of it the first time, and again when
//! an older build wrote it since.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Machine, Refused, Store, WindowId, file, migrate};

pub struct Host {
    pub store: Store,
    dir: PathBuf,
    _lock: std::fs::File,
}

/// What the start found to tell the user.
pub type Note = Option<String>;

fn as_machine(state: &Value) -> Result<Machine, String> {
    migrate::from_state(state)
}

impl Host {
    /// The store in `dir`, the window's state.json (`state`, at `state_file`,
    /// marked with the store's rev `machine_rev` it was written at) read
    /// against it. None when another process keeps it.
    /// `wait`: for a lock let go of a moment from now (a daemon handed
    /// over: the one before lets go of it as it exits).
    pub fn open(dir: &Path, state_file: &Path, state: &Value, machine_rev: u64, wait: bool) -> Result<(Host, Note), String> {
        let mut lock = file::lock(dir)?;
        for _ in 0..if wait { 40 } else { 0 } {
            if lock.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            lock = file::lock(dir)?;
        }
        let lock = lock.ok_or_else(|| format!("another ideae keeps this machine's store ({})", dir.display()))?;
        let found = file::load(dir)?;
        let keep = |what: &str| {
            let to = dir.join(format!("machine.json.{what}-{}", crate::paths::now()));
            let _ = std::fs::copy(file::path(dir), &to);
            to
        };
        let mut note = None;
        let machine = match found {
            // The first start of a build with a store: made of state.json, kept as it was.
            None => {
                let before = dir.join("state.json.before-machine");
                if !before.exists() {
                    let _ = std::fs::copy(state_file, &before);
                }
                fresh(dir, as_machine(state).map_err(|e| format!("the store not made: {e}"))?)
            }
            Some(machine) if machine_rev == machine.rev => machine,
            // Written by this build up to a crash between the store and state.json: the store is the later.
            Some(machine) if machine_rev != 0 && machine_rev < machine.rev => machine,
            // An older build ran since, the store untouched meanwhile (or a store older than state.json): state.json is what is.
            Some(machine) if machine_rev == 0 && machine.rev == machine.mirrored || machine_rev > machine.rev => match as_machine(state) {
                Ok(mut again) => {
                    let kept = keep("before-reimport");
                    again.rev = machine.rev + 1;
                    keep_ids(&mut again, &machine);
                    eprintln!("ideae: state.json was written by an older ideae: the store made of it again (the one before: {})", kept.display());
                    fresh(dir, again)
                }
                Err(e) => {
                    eprintln!("ideae: {e}");
                    machine
                }
            },
            // Both changed: the store's, and state.json kept aside.
            Some(machine) => {
                let aside = dir.join(format!("state.json.not-merged-{}", crate::paths::now()));
                let _ = std::fs::copy(state_file, &aside);
                note = Some(format!("An older ideae ran since this one last did, and both changed your tasks. This one's are shown; the older one's state is kept at {}.", aside.display()));
                machine
            }
        };
        Ok((Host { store: Store::new(machine), dir: dir.to_path_buf(), _lock: lock }, note))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn machine(&self) -> &Machine {
        &self.store.machine
    }

    /// An id given now (`Op::ReserveCrew`, `Op::ReserveQuestion`), written
    /// at once: a restart never gives it again.
    pub fn reserve(&mut self, by: WindowId, op: super::Op) -> Result<u32, Refused> {
        let n = match self.store.apply(by, op)? {
            super::Applied::Crew(n) | super::Applied::Question(n) => n,
            other => return Err(Refused::Invalid(format!("not an id: {other:?}"))),
        };
        if let Err(e) = file::save(&self.dir, &self.store.machine) {
            eprintln!("ideae: the store not written: {e}");
        }
        Ok(n)
    }

    /// Brings into the store what window `by` changed from `base` to
    /// `wanted` (see sync.rs) and writes it when that changed it. Its rev
    /// now, and what it refused. `mirrors`: the window is the one whose
    /// state.json is beside the store, written with that rev (not one of
    /// another machine): what an older build's state.json is read against.
    pub fn sync(&mut self, by: WindowId, base: &Machine, wanted: &Machine, mirrors: bool) -> (u64, Vec<Refused>) {
        let before = self.store.machine.rev;
        let refused = super::sync::sync(&mut self.store, by, base, wanted);
        let machine = &mut self.store.machine;
        let mirrored = mirrors && machine.mirrored != machine.rev;
        if mirrored {
            machine.mirrored = machine.rev;
        }
        if mirrored || machine.rev != before {
            if let Err(e) = file::save(&self.dir, machine) {
                eprintln!("ideae: the store not written: {e}");
            }
        }
        (machine.rev, refused)
    }
}

/// The ids `before` gave its tasks kept by path, the new ones above all it
/// ever gave: an id is never given twice.
fn keep_ids(again: &mut Machine, before: &Machine) {
    let mut last = before.last_task.max(again.last_task);
    for task in again.projects.iter_mut().flat_map(|p| &mut p.tasks) {
        task.id = match before.projects.iter().flat_map(|p| &p.tasks).find(|t| t.path == task.path) {
            Some(was) => was.id,
            None => {
                last += 1;
                last
            }
        };
    }
    again.last_task = last;
    again.last_crew = again.last_crew.max(before.last_crew);
}

/// A store made now: written at once, its rev the mirror's.
fn fresh(dir: &Path, mut machine: Machine) -> Machine {
    machine.rev = machine.rev.max(1);
    machine.mirrored = machine.rev;
    if let Err(e) = file::save(dir, &machine) {
        eprintln!("ideae: the store not written: {e}");
    }
    machine
}
