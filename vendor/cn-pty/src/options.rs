//! Options for a local terminal's pty.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Where a local terminal starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WorkingDirectory {
    /// The host process's working directory.
    #[default]
    Inherit,
    /// The user's home directory.
    Home,
    /// A path; a missing path falls back to home, then the root.
    Path(PathBuf),
}

/// Which shell integration script to inject.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellIntegration {
    /// Pick by the program's file name.
    #[default]
    Detect,
    /// Inject nothing.
    None,
    /// Bash through `--posix` and `ENV`.
    Bash,
    /// Zsh through `ZDOTDIR`.
    Zsh,
    /// Fish through `XDG_DATA_DIRS`.
    Fish,
    /// Elvish through `XDG_DATA_DIRS`.
    Elvish,
    /// Nushell through `XDG_DATA_DIRS` and `--execute`.
    Nushell,
}

/// Options for a local terminal with its own pty.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalTerminalOptions {
    /// The program; `None` runs the user's shell.
    pub(crate) program: Option<PathBuf>,
    /// Arguments for the program.
    pub(crate) args: Vec<String>,
    /// Working directory.
    pub(crate) cwd: WorkingDirectory,
    /// Extra environment variables.
    pub(crate) env: BTreeMap<String, String>,
    /// Environment variables to remove.
    pub(crate) env_remove: Vec<String>,
    /// Bytes written to the program right after it starts.
    pub(crate) input: Vec<u8>,
    /// Shell integration.
    pub(crate) shell_integration: ShellIntegration,
    /// Shell integration features, as `GHOSTTY_SHELL_FEATURES`.
    pub(crate) shell_integration_features: Vec<String>,
    /// Start the shell as a login shell.
    pub(crate) login: bool,
    /// An exit faster than this is reported as abnormal.
    pub(crate) abnormal_exit_runtime: Duration,
}

impl Default for LocalTerminalOptions {
    fn default() -> Self {
        Self {
            program: None,
            args: Vec::new(),
            cwd: WorkingDirectory::Inherit,
            env: BTreeMap::new(),
            env_remove: Vec::new(),
            input: Vec::new(),
            shell_integration: ShellIntegration::Detect,
            shell_integration_features: vec![
                "cursor".to_owned(),
                "sudo".to_owned(),
                "title".to_owned(),
            ],
            login: true,
            abnormal_exit_runtime: Duration::from_millis(250),
        }
    }
}

impl LocalTerminalOptions {
    /// Set the program and its arguments.
    #[must_use]
    pub fn with_program(
        mut self,
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.program = Some(program.into());
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: WorkingDirectory) -> Self {
        self.cwd = cwd;
        self
    }

    /// Add an environment variable.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Set the shell integration.
    #[must_use]
    pub fn with_shell_integration(mut self, integration: ShellIntegration) -> Self {
        self.shell_integration = integration;
        self
    }

    /// Set whether the shell starts as a login shell.
    #[must_use]
    pub fn with_login(mut self, login: bool) -> Self {
        self.login = login;
        self
    }

    /// Set the bytes written right after the program starts.
    #[must_use]
    pub fn with_input(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.input = input.into();
        self
    }

    /// Remove an environment variable from the program's environment.
    #[must_use]
    pub fn without_env(mut self, name: impl Into<String>) -> Self {
        self.env_remove.push(name.into());
        self
    }

    /// Set the shell integration features, as `GHOSTTY_SHELL_FEATURES`.
    #[must_use]
    pub fn with_shell_integration_features(
        mut self,
        features: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.shell_integration_features = features.into_iter().map(Into::into).collect();
        self
    }

    /// Set the run time under which an exit is reported as abnormal.
    #[must_use]
    pub fn with_abnormal_exit_runtime(mut self, runtime: Duration) -> Self {
        self.abnormal_exit_runtime = runtime;
        self
    }

    /// The program; `None` runs the user's shell.
    pub fn program(&self) -> Option<&std::path::Path> {
        self.program.as_deref()
    }

    /// The program's arguments.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The working directory.
    pub fn cwd(&self) -> &WorkingDirectory {
        &self.cwd
    }

    /// Extra environment variables.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Environment variables removed from the program's environment.
    pub fn env_remove(&self) -> &[String] {
        &self.env_remove
    }

    /// The bytes written right after the program starts.
    pub fn input(&self) -> &[u8] {
        &self.input
    }

    /// The shell integration.
    pub fn shell_integration(&self) -> ShellIntegration {
        self.shell_integration
    }

    /// The shell integration features.
    pub fn shell_integration_features(&self) -> &[String] {
        &self.shell_integration_features
    }

    /// Whether the shell starts as a login shell.
    pub fn login(&self) -> bool {
        self.login
    }

    /// The run time under which an exit is reported as abnormal.
    pub fn abnormal_exit_runtime(&self) -> Duration {
        self.abnormal_exit_runtime
    }
}

