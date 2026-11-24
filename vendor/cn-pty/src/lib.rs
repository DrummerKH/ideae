//! ideae patch: a local program in a pty, as gpui-cn's terminal starts it,
//! with nothing of a window: the shell and its environment set up the way
//! Ghostty does (its terminfo and shell integration scripts are embedded
//! here), the process in the foreground, and the handle the program's
//! output and input go through. gpui-cn re-exports all of it; a daemon
//! that runs terminals for a window elsewhere uses it alone.

mod options;
#[cfg(all(unix, not(any(target_os = "ios", target_os = "android"))))]
mod process;
#[cfg(windows)]
mod process_windows;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod pty;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod pty_io;
pub mod resources;
pub mod shell;
mod sink;
mod types;

pub use options::{LocalTerminalOptions, ShellIntegration, WorkingDirectory};
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub use pty::LocalPty;
#[cfg(unix)]
pub use pty::adopt;
pub use sink::{Detached, Output, Sink, SinkBatch};
pub use types::{ByteHandle, ExitStatus, ForegroundProcess, MAX_GRID, Viewport};
