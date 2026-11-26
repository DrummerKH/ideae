//! Production connections: a safety catch. Nothing runs on one until
//! production is unlocked, which asks first; unlocked, all of them run for
//! a while and lock again by themselves. It keeps a statement from going to
//! production by mistake; it is no secret, and keeps nothing from a program
//! that has the connection's login.

use std::{
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

use super::config::{self, Connection};

pub const LOCKED: &str = "Production is locked: unlock it to run this";
/// How long production stays unlocked; [`MANUAL`] until it is locked again by hand.
pub const MINUTES: [u32; 7] = [5, 10, 15, 30, 60, 120, MANUAL];
pub const MANUAL: u32 = 0;

struct Open {
    /// When it locks again by itself, and the same moment on the clock, to
    /// show; never when unlocked by hand.
    until: Option<(Instant, SystemTime)>,
}

static OPEN: Mutex<Option<Open>> = Mutex::new(None);

/// Locked again once its time is up. Both clocks are asked: `Instant`
/// stands still while the machine sleeps, the wall clock does not.
pub fn is_open() -> bool {
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if open.as_ref().and_then(|o| o.until).is_some_and(|(until, clock)| Instant::now() >= until || SystemTime::now() >= clock) {
        *open = None;
    }
    open.is_some()
}

/// Until when it is unlocked, on the clock; none while locked or unlocked by hand.
pub fn open_until() -> Option<SystemTime> {
    is_open().then(|| OPEN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|o| o.until).map(|(_, clock)| clock)).flatten()
}

/// While unlocked, until when: "until 14:30", or until it is locked by hand.
pub fn unlocked_until() -> Option<String> {
    is_open().then(|| match open_until() {
        Some(until) => format!("until {}", crate::local_time::hour_minute(until)),
        None => "until you lock it".into(),
    })
}

/// How long an unlock lasts, as a choice says it: "10 min", "1 h", "Manual".
pub fn lasts_label(minutes: u32) -> String {
    match minutes {
        MANUAL => "Manual".into(),
        m if m % 60 == 0 => format!("{} h", m / 60),
        m => format!("{m} min"),
    }
}

/// For how long an unlock lasts, in a sentence: "for 10 minutes", "until you lock it".
pub fn lasts() -> String {
    match config::production_minutes() {
        MANUAL => "until you lock it again".into(),
        60 => "for an hour".into(),
        m if m % 60 == 0 => format!("for {} hours", m / 60),
        m => format!("for {m} minutes"),
    }
}

/// How long until it locks again by itself.
pub fn left() -> Option<Duration> {
    is_open()
        .then(|| {
            OPEN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|o| o.until).map(|(until, clock)| {
                let by_clock = clock.duration_since(SystemTime::now()).unwrap_or_default();
                until.saturating_duration_since(Instant::now()).min(by_clock)
            })
        })
        .flatten()
}

pub(super) fn unlock_for(lasts: Option<Duration>) {
    let until = lasts.map(|lasts| (Instant::now() + lasts, SystemTime::now() + lasts));
    *OPEN.lock().unwrap_or_else(|e| e.into_inner()) = Some(Open { until });
}

/// Unlocked for the time chosen, or until locked by hand. Each start of the app is locked.
pub fn unlock() {
    let minutes = config::production_minutes();
    unlock_for((minutes != MANUAL).then(|| Duration::from_secs(u64::from(minutes) * 60)));
}

pub fn lock() {
    *OPEN.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether statements may go to this connection now. Its profile as it is
/// now, not as it was when a session took its copy.
pub fn may_run(connection: &Connection) -> Result<(), String> {
    let production = config::find(&connection.id).map_or(connection.production, |now| now.production);
    if production && !is_open() { Err(LOCKED.into()) } else { Ok(()) }
}

/// Tests that lock or unlock hold this: the state is the process's.
#[cfg(test)]
pub static TESTS: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_runs_only_while_unlocked_and_locks_again_on_time() {
        let _one = TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let connection = |production| Connection {
            id: "c".into(),
            name: "c".into(),
            driver: Default::default(),
            host: "h".into(),
            port: 1,
            user: String::new(),
            database: None,
            bastion: None,
            ssh: None,
            tls: Default::default(),
            color: None,
            production,
        };
        lock();
        assert_eq!(may_run(&connection(true)), Err(LOCKED.into()));
        assert_eq!(may_run(&connection(false)), Ok(()));
        unlock_for(Some(Duration::from_secs(60)));
        assert!(may_run(&connection(true)).is_ok() && open_until().is_some() && left().is_some());
        unlock_for(Some(Duration::ZERO));
        assert_eq!(may_run(&connection(true)), Err(LOCKED.into()));
        // By hand: open with no end, until locked.
        unlock_for(None);
        assert!(may_run(&connection(true)).is_ok() && open_until().is_none() && left().is_none());
        assert_eq!(unlocked_until().as_deref(), Some("until you lock it"));
        lock();
        assert_eq!(may_run(&connection(true)), Err(LOCKED.into()));
        assert_eq!((lasts_label(5), lasts_label(60), lasts_label(120), lasts_label(MANUAL)), ("5 min".into(), "1 h".into(), "2 h".into(), "Manual".into()));
    }
}
