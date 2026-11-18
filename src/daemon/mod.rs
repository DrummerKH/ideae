//! Terminals that outlive the window: a daemon process runs them, windows
//! attach. Unix only; elsewhere terminals stay in the window.

pub mod client;
pub mod pipe;
pub mod proto;
pub mod server;
