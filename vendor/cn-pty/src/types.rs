//! What a terminal's program is told and tells: how it ended, what runs in
//! its foreground, its grid, and the handle a source gives for its input.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// How a program ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExitStatus {
    pub(crate) code: Option<i32>,
    pub(crate) signal: Option<String>,
    pub(crate) runtime: Duration,
    pub(crate) abnormal: bool,
}

impl ExitStatus {
    /// An exit with `code`, or by `signal` when a signal ended the program.
    /// The engine fills in the run time.
    pub fn new(code: Option<i32>, signal: Option<impl Into<String>>) -> Self {
        Self {
            code,
            signal: signal.map(Into::into),
            runtime: Duration::ZERO,
            abnormal: false,
        }
    }

    /// The exit code, `None` when a signal ended the program.
    pub fn code(&self) -> Option<i32> {
        self.code
    }

    /// The signal name, when a signal ended the program.
    pub fn signal(&self) -> Option<&str> {
        self.signal.as_deref()
    }

    /// How long the program ran.
    pub fn runtime(&self) -> Duration {
        self.runtime
    }

    /// Whether the run was shorter than the abnormal-exit threshold.
    pub fn is_abnormal(&self) -> bool {
        self.abnormal
    }
}

/// The process in the foreground of a terminal: the one that reads its
/// keys, such as the shell at its prompt or the editor it started.
///
/// A local pty reads it from the process table: the terminal's foreground
/// process group (`tcgetpgrp`), then the name, arguments and working
/// directory of that group's leader, through `proc_pidinfo` and `sysctl`
/// on macOS and `/proc` on Linux. Another [`ByteSource`] reports its own,
/// or none.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ForegroundProcess {
    pub(crate) pid: u32,
    pub(crate) name: String,
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) shell: bool,
}

impl ForegroundProcess {
    /// Process `pid`, named `name`, such as `vim`.
    pub fn new(pid: u32, name: impl Into<String>) -> Self {
        Self {
            pid,
            name: name.into(),
            argv: Vec::new(),
            cwd: None,
            shell: false,
        }
    }

    /// Set the arguments, the program's own name first.
    #[must_use]
    pub fn with_argv(mut self, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.argv = argv.into_iter().map(Into::into).collect();
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set whether it is the program the terminal started, such as the
    /// shell at its prompt.
    #[must_use]
    pub fn with_shell(mut self, shell: bool) -> Self {
        self.shell = shell;
        self
    }

    /// The process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The executable's name, such as `vim`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The arguments, the program's own name first, when they could be
    /// read.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The working directory, when it could be read.
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// Whether it is the program the terminal started, such as the shell at
    /// its prompt, rather than a program that program runs. An application
    /// asks before it closes a terminal where this is false.
    pub fn is_shell(&self) -> bool {
        self.shell
    }
}


impl ExitStatus {
    /// ideae patch: the engine fills in how long the program ran, and
    /// whether that was short enough to count as abnormal.
    #[doc(hidden)]
    pub fn set_run(&mut self, runtime: Duration, abnormal_under: Duration) {
        self.runtime = runtime;
        self.abnormal = runtime <= abnormal_under;
    }
}

/// The engine's side of a byte source.
pub trait ByteHandle: Send {
    /// Write input for the program without blocking: as much of `bytes` as
    /// it takes now, and return how much that was. When that is less than
    /// all of it, the source calls [`crate::Output::writable`] once more fits,
    /// and the engine writes the rest then, ahead of newer input.
    fn write(&self, bytes: &[u8]) -> io::Result<usize>;
    /// The grid changed.
    fn resize(&self, viewport: Viewport) -> io::Result<()>;
    /// Whether the program ended, and how.
    fn try_wait(&self) -> io::Result<Option<ExitStatus>>;
    /// Stop the program and release the source.
    fn close(&self);
    /// The process in the foreground, if the source knows it. The engine
    /// asks on its own thread, at most every 50 ms and only after output,
    /// so it may read the process table but must not block.
    fn foreground(&self) -> Option<ForegroundProcess> {
        None
    }

    /// ideae patch: the pty's master descriptor and the program's pid, for
    /// a process that hands the running program to another one.
    #[cfg(unix)]
    fn pty(&self) -> Option<(std::os::fd::RawFd, u32)> {
        None
    }
}


/// The grid size and the cell size in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Viewport {
    columns: u16,
    rows: u16,
    cell_width: u32,
    cell_height: u32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
        }
    }
}

/// The largest grid the engine accepts on either axis.
pub const MAX_GRID: u16 = 1000;

impl Viewport {
    /// A viewport, rejecting empty grids, grids over [`MAX_GRID`] and cells
    /// over 4096 pixels.
    pub fn new(columns: u16, rows: u16, cell_width: u32, cell_height: u32) -> Option<Self> {
        if columns == 0
            || rows == 0
            || columns > MAX_GRID
            || rows > MAX_GRID
            || cell_width == 0
            || cell_height == 0
            || cell_width > 4096
            || cell_height > 4096
        {
            return None;
        }
        Some(Self {
            columns,
            rows,
            cell_width,
            cell_height,
        })
    }
    /// Columns.
    #[must_use]
    pub fn columns(self) -> u16 {
        self.columns
    }
    /// Rows.
    #[must_use]
    pub fn rows(self) -> u16 {
        self.rows
    }
    /// Cell width in device pixels.
    #[must_use]
    pub fn cell_width(self) -> u32 {
        self.cell_width
    }
    /// Cell height in device pixels.
    #[must_use]
    pub fn cell_height(self) -> u32 {
        self.cell_height
    }
}

