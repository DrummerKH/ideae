//! What runs where there is no window: the daemon that keeps a machine's
//! terminals and agents, the bridge ssh runs to reach it from a window on
//! another machine, and what the two sides need to agree on. The app links
//! this and re-exports it where it was; `ideae-server` is built from it
//! alone, with no gpui, for a server to run.

pub mod build;
#[cfg(unix)]
pub mod daemon;
pub mod hook;
pub mod paths;
pub mod procs;
pub mod said;
pub mod status;

#[cfg(test)]
pub(crate) mod tests {
    /// Tests that set IDEAE_STATE run one at a time.
    pub static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
