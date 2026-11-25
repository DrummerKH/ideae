//! What chat agents cost today, by profile, as the agents report it (ACP's
//! `usage_update` cost of a session, in USD): for a profile's daily limit.
//! Kept in `spend.json` next to the state; a session's cost is counted by
//! what it grew since last seen, so one that ran while no window was open
//! is counted when a window sees it again.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// Sessions remembered, the last ones: their cost grows from there.
const SESSIONS_KEPT: usize = 500;

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
struct Book {
    /// The day (UTC, days since 1970) the totals are of.
    day: u64,
    by_profile: BTreeMap<String, f64>,
    /// A session's cost when last seen, and when (for keeping the last).
    seen: BTreeMap<String, (f64, u64)>,
}

fn path() -> std::path::PathBuf {
    crate::state::state_dir().join("spend.json")
}

fn today() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86_400)
}

fn book() -> &'static Mutex<Book> {
    static BOOK: OnceLock<Mutex<Book>> = OnceLock::new();
    BOOK.get_or_init(|| {
        let book = if cfg!(test) { None } else { std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()) };
        Mutex::new(book.unwrap_or_default())
    })
}

/// Read off the thread that asks first.
pub fn preload() {
    std::thread::spawn(|| {
        let _ = book();
    });
}

/// A cost as the host shows it ("$0.42"), in USD; None for another currency.
pub use ideae_core::acp::usd;

/// A session of `profile` now costs `cost` (USD, all of it so far): its
/// growth is added to today's. Returns today's total for the profile.
pub fn record(profile: &str, session: &str, cost: f64) -> f64 {
    let mut book = book().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let day = today();
    if book.day != day {
        book.day = day;
        book.by_profile.clear();
    }
    let before = book.seen.get(session).map_or(0.0, |(c, _)| *c);
    // Less than before: the agent counts it anew (a new session of that id).
    let grown = if cost >= before { cost - before } else { cost };
    book.seen.insert(session.to_string(), (cost, day));
    if book.seen.len() > SESSIONS_KEPT {
        let oldest = book.seen.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone());
        if let Some(oldest) = oldest {
            book.seen.remove(&oldest);
        }
    }
    let total = book.by_profile.entry(profile.to_string()).or_default();
    *total += grown;
    let total = *total;
    if grown > 0.0 && !cfg!(test) {
        let copy = book.clone();
        std::thread::spawn(move || write(&copy));
    }
    total
}

/// What `profile` has cost today.
pub fn spent_today(profile: &str) -> f64 {
    let book = book().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if book.day != today() {
        return 0.0;
    }
    book.by_profile.get(profile).copied().unwrap_or(0.0)
}

fn write(book: &Book) {
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok(json) = serde_json::to_vec(book) else { return };
    let path = path();
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_session_counts_what_it_grew() {
        assert_eq!(super::usd("$0.42"), Some(0.42));
        assert_eq!(super::usd("0.42 EUR"), None);
        let profile = format!("p{}", std::process::id());
        let start = super::spent_today(&profile);
        super::record(&profile, "s1", 0.10);
        super::record(&profile, "s1", 0.25);
        // Seen again later, unchanged: nothing more.
        super::record(&profile, "s1", 0.25);
        let total = super::record(&profile, "s2", 0.05);
        assert!((total - start - 0.30).abs() < 1e-9, "{total}");
    }
}
