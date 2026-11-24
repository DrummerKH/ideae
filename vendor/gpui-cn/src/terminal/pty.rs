//! ideae patch: the pty is cn-pty's, which needs no window; this makes it
//! the engine's byte source.

use std::io;

use crate::terminal::engine::{ByteHandle, ByteSink, ByteSource};
use crate::terminal::frame::Viewport;
use crate::terminal::options::LocalTerminalOptions;

/// A program to run in a local pty.
#[derive(Debug)]
pub struct LocalPty(cn_pty::LocalPty);

impl LocalPty {
    /// A pty for `options`.
    #[must_use]
    pub fn new(options: LocalTerminalOptions) -> Self {
        Self(cn_pty::LocalPty::new(options.pty))
    }
}

impl ByteSource for LocalPty {
    fn start(self: Box<Self>, sink: ByteSink, viewport: Viewport) -> io::Result<Box<dyn ByteHandle>> {
        self.0.start(sink, viewport)
    }
}

/// Takes over a program running in a pty another process started: its
/// master descriptor and its pid.
#[cfg(unix)]
pub fn adopt(
    master: std::os::fd::OwnedFd,
    pid: u32,
    program: Option<String>,
    sink: ByteSink,
) -> io::Result<Box<dyn ByteHandle>> {
    cn_pty::adopt(master, pid, program, sink)
}

impl cn_pty::Output for ByteSink {
    fn write(&self, bytes: &[u8]) -> bool {
        ByteSink::write(self, bytes)
    }

    fn wait_for_room(&self) -> bool {
        ByteSink::wait_for_room(self)
    }

    fn writable(&self) {
        ByteSink::writable(self);
    }

    fn finished(&self) {
        ByteSink::finished(self);
    }
}
