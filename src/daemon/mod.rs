//! Terminals that outlive the window: a daemon process runs them (see
//! `ideae_core::daemon`), windows attach. Unix only; elsewhere terminals
//! stay in the window.

pub use ideae_core::daemon::{bridge, proto, server};
pub mod client;
pub mod install;
pub mod remote;

/// No window to show it: an agent that wants the user says so on the
/// desktop. The daemon calls it ([`server::NOTIFY`]).
pub fn notify(hook: &proto::Hook) {
    // The window may have changed it since this daemon started.
    let saved = crate::state::load();
    if !saved.settings.notifications || !saved.tells_of(std::path::Path::new(&hook.task)) {
        return;
    }
    let Some(status) = hook.event().status().filter(|s| s.is_attention()) else { return };
    let words = saved.settings.notify_said && saved.settings.show_said && saved.presenting.is_none();
    let said = hook.said.as_deref().filter(|_| words);
    let body = crate::said::notice(&status, said, words);
    let task = std::path::Path::new(&hook.task).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let path = std::path::PathBuf::from(&hook.task);
    crate::desktop::notify_with(&task, &body, move || crate::desktop::open_window_on(&path));
}
