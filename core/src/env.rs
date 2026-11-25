//! Variables an agent is given: a value that is a command (`$(op read …)`)
//! is what that command prints, run where the agent runs.

/// The command of a value written `$(…)`.
pub fn command_value(value: &str) -> Option<&str> {
    value.trim().strip_prefix("$(")?.strip_suffix(')').map(str::trim).filter(|c| !c.is_empty())
}

/// Variables made by a command (`$(pass show key)`) given their values:
/// the command's output, its last line end dropped. Not for the UI thread.
pub fn run_made(env: Vec<(String, String)>) -> Vec<(String, String)> {
    env.into_iter()
        .filter_map(|(k, v)| match command_value(&v) {
            None => Some((k, v)),
            Some(made) => {
                let mut command = if cfg!(windows) {
                    let mut c = crate::procs::command("cmd");
                    c.args(["/C", made]);
                    c
                } else {
                    let mut c = std::process::Command::new("sh");
                    c.args(["-c", made]);
                    c
                };
                let out = command.stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
                let value = String::from_utf8_lossy(&out.stdout).trim_end_matches(['\n', '\r']).to_string();
                (out.status.success() && !value.is_empty()).then_some((k, value))
            }
        })
        .collect()
}
