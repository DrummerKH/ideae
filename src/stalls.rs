//! A debug aid: tells when the UI thread did not come round for a while.
//! In debug builds and under `IDEAE_DEBUG`: to stderr, and to stalls.log
//! next to the state. It tells how long, not what held it: for that, the
//! time in the log against what was done then.

use std::{
    io::Write as _,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui_kit::App;

/// How often the UI thread says it is there, and how long without it is told.
const BEAT: Duration = Duration::from_millis(50);
const HELD: Duration = Duration::from_millis(100);

pub fn watch(cx: &mut App) {
    if !cfg!(debug_assertions) && std::env::var_os("IDEAE_DEBUG").is_none() {
        return;
    }
    let started = Instant::now();
    let beat = Arc::new(AtomicU64::new(0));
    let seen = beat.clone();
    cx.spawn(async move |cx| {
        loop {
            beat.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            cx.background_executor().timer(BEAT).await;
        }
    })
    .detach();
    let _ = std::thread::Builder::new().name("stalls".into()).spawn(move || {
        // The beat a stall began at, so that one stall is told once, when it ends.
        let mut held_since: Option<u64> = None;
        loop {
            std::thread::sleep(BEAT);
            let (now, last) = (started.elapsed().as_millis() as u64, seen.load(Ordering::Relaxed));
            match held_since {
                None if now.saturating_sub(last) > (BEAT + HELD).as_millis() as u64 => held_since = Some(last),
                Some(since) if last != since => {
                    held_since = None;
                    let line = format!("UI thread held for {} ms, until {:.1} s after the start", last - since, last as f64 / 1000.);
                    eprintln!("{line}");
                    let dir = crate::state::state_dir();
                    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("stalls.log")) {
                        let _ = writeln!(log, "{} {line}", crate::state::now());
                    }
                }
                _ => {}
            }
        }
    });
}
