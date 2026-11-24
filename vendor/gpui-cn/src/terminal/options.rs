//! Options for the engine and for a local terminal.

use std::time::Duration;

/// Engine limits, with Ghostty's defaults: 10,000,000 bytes of scrollback
/// (`scrollback-limit` in `ghostty +show-config --default`), so a busy
/// terminal holds as much history as Ghostty's and no more.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EngineOptions {
    /// Maximum scrollback in bytes. Zero disables scrollback.
    pub(crate) scrollback_bytes: usize,
    /// Maximum scrollback in lines; `None` is unlimited.
    pub(crate) scrollback_lines: Option<usize>,
    /// The terminfo name reported to XTGETTCAP.
    pub(crate) terminfo_name: String,
    /// Maximum decoded bytes per Kitty clipboard write.
    pub(crate) clipboard_write_max_bytes: usize,
    /// Bytes of Kitty graphics images each screen may hold. Zero turns the
    /// protocol off.
    pub(crate) image_storage_bytes: u64,
    pub(crate) park: crate::terminal::park::ParkOptions,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            scrollback_bytes: 10_000_000,
            scrollback_lines: None,
            terminfo_name: ghostty_vt::terminfo::TERM.to_owned(),
            clipboard_write_max_bytes: 64 << 20,
            // libghostty-vt's default.
            image_storage_bytes: 10_000_000,
            park: crate::terminal::park::ParkOptions::default(),
        }
    }
}

impl EngineOptions {
    /// Set the scrollback byte limit.
    #[must_use]
    pub fn with_scrollback_bytes(mut self, bytes: usize) -> Self {
        self.scrollback_bytes = bytes;
        self
    }

    /// Set the scrollback line limit.
    #[must_use]
    pub fn with_scrollback_lines(mut self, lines: Option<usize>) -> Self {
        self.scrollback_lines = lines;
        self
    }

    /// Set how many bytes of Kitty graphics images each screen may hold.
    /// The default is 10 MB, libghostty-vt's default; zero turns the Kitty
    /// graphics protocol off.
    #[must_use]
    pub fn with_image_storage_bytes(mut self, bytes: u64) -> Self {
        self.image_storage_bytes = bytes;
        self
    }

    /// Set the terminfo name.
    #[must_use]
    pub fn with_terminfo_name(mut self, name: impl Into<String>) -> Self {
        self.terminfo_name = name.into();
        self
    }

    /// Set the largest decoded Kitty clipboard write.
    #[must_use]
    pub fn with_clipboard_write_max_bytes(mut self, bytes: usize) -> Self {
        self.clipboard_write_max_bytes = bytes;
        self
    }

    /// Set when an idle terminal parks and where its snapshot goes. On by
    /// default, after 60 seconds idle, in
    /// memory.
    #[must_use]
    pub fn with_park(mut self, park: crate::terminal::park::ParkOptions) -> Self {
        self.park = park;
        self
    }

    /// When an idle terminal parks and where its snapshot goes.
    pub fn park(&self) -> &crate::terminal::park::ParkOptions {
        &self.park
    }

    /// The scrollback byte limit. Zero disables scrollback.
    pub fn scrollback_bytes(&self) -> usize {
        self.scrollback_bytes
    }

    /// The scrollback line limit; `None` is unlimited.
    pub fn scrollback_lines(&self) -> Option<usize> {
        self.scrollback_lines
    }

    /// The terminfo name reported to XTGETTCAP.
    pub fn terminfo_name(&self) -> &str {
        &self.terminfo_name
    }

    /// The largest decoded Kitty clipboard write.
    pub fn clipboard_write_max_bytes(&self) -> usize {
        self.clipboard_write_max_bytes
    }
}

/// Options for a local terminal with its own pty: the engine's limits and
/// the pty's own options (ideae patch: those are cn-pty's, which needs no
/// window). The pty's are read through `Deref`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalTerminalOptions {
    /// Engine limits.
    pub(crate) engine: EngineOptions,
    pub(crate) pty: cn_pty::LocalTerminalOptions,
}

impl LocalTerminalOptions {
    /// Set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: WorkingDirectory) -> Self {
        self.pty = self.pty.with_cwd(cwd);
        self
    }

    /// Add an environment variable.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.pty = self.pty.with_env(name, value);
        self
    }

    /// Set the shell integration.
    #[must_use]
    pub fn with_shell_integration(mut self, integration: ShellIntegration) -> Self {
        self.pty = self.pty.with_shell_integration(integration);
        self
    }

    /// Set whether the shell starts as a login shell.
    #[must_use]
    pub fn with_login(mut self, login: bool) -> Self {
        self.pty = self.pty.with_login(login);
        self
    }

    /// Set the bytes written right after the program starts.
    #[must_use]
    pub fn with_input(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.pty = self.pty.with_input(input);
        self
    }

    /// Remove an environment variable from the program's environment.
    #[must_use]
    pub fn without_env(mut self, name: impl Into<String>) -> Self {
        self.pty = self.pty.without_env(name);
        self
    }

    /// Set the run time under which an exit is reported as abnormal.
    #[must_use]
    pub fn with_abnormal_exit_runtime(mut self, runtime: Duration) -> Self {
        self.pty = self.pty.with_abnormal_exit_runtime(runtime);
        self
    }

    /// Set the program and its arguments.
    #[must_use]
    pub fn with_program(
        mut self,
        program: impl Into<std::path::PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.pty = self.pty.with_program(program, args);
        self
    }

    /// Set the shell integration features, as `GHOSTTY_SHELL_FEATURES`.
    #[must_use]
    pub fn with_shell_integration_features(
        mut self,
        features: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.pty = self.pty.with_shell_integration_features(features);
        self
    }

    /// Set the engine options.
    #[must_use]
    pub fn with_engine(mut self, engine: EngineOptions) -> Self {
        self.engine = engine;
        self
    }

    /// The engine options.
    pub fn engine(&self) -> &EngineOptions {
        &self.engine
    }

    /// The pty's options.
    pub fn pty(&self) -> &cn_pty::LocalTerminalOptions {
        &self.pty
    }
}

impl std::ops::Deref for LocalTerminalOptions {
    type Target = cn_pty::LocalTerminalOptions;

    fn deref(&self) -> &Self::Target {
        &self.pty
    }
}

pub use cn_pty::{ShellIntegration, WorkingDirectory};

#[cfg(test)]
mod tests {
    #[test]
    fn the_scrollback_limit_is_ghosttys_default() {
        assert_eq!(
            super::EngineOptions::default().scrollback_bytes(),
            10_000_000
        );
    }
}
