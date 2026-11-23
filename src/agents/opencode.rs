//! OpenCode, from its source (github.com/anomalyco/opencode, v1.19.6,
//! packages/), not tried live:
//! - Plugins see the server's events (opencode/src/plugin/index.ts):
//!   `session.status` busy/idle, `permission.asked`, `question.asked`, each
//!   with `properties.sessionID`. A plugin of the app's is added for the
//!   session with `OPENCODE_CONFIG_CONTENT='{"plugin":["file://…"]}'`, which
//!   merges with the user's config (config/config.ts); their files stay as
//!   they are. A profile that sets that variable itself keeps it, and then
//!   reports nothing.
//! - Session ids (`ses_…`) cannot be chosen; `--session <id>` resumes one,
//!   `--continue` the latest (cli/cmd/tui.ts).
//! - `--auto` answers its permission questions (cli/cmd/run.ts, tui).
//! - Sessions are kept in SQLite (opencode.db): what they used is not read.
//! - `opencode run` prints the answer's text on stdout; questions of
//!   permission are refused there.

use super::*;

pub struct OpenCode;

const CONTENT_ENV: &str = "OPENCODE_CONFIG_CONTENT";

/// The plugin: each event of the session in this folder runs `ideae --hook`
/// with what Claude Code's hooks would say.
pub fn plugin_source(exe: &Path) -> String {
    let exe = serde_json::to_string(&exe.display().to_string()).unwrap_or_default();
    format!(
        r#"// Written by ideae: tells the app what the OpenCode of a task does.
import {{ spawn }} from "node:child_process";
const exe = {exe};
function tell(event, payload) {{
  if (!process.env.IDEAE_SOCK || !process.env.IDEAE_TASK) return;
  try {{
    const child = spawn(exe, ["--hook", event], {{ stdio: ["pipe", "ignore", "ignore"] }});
    child.on("error", () => {{}});
    child.stdin.on("error", () => {{}});
    child.stdin.end(JSON.stringify(payload));
  }} catch {{}}
}}
export const IdeaePlugin = async () => ({{
  event: async ({{ event }}) => {{
    const p = (event && event.properties) || {{}};
    const session_id = p.sessionID;
    switch (event && event.type) {{
      case "session.status":
        if (p.status && p.status.type === "busy") tell("UserPromptSubmit", {{ session_id }});
        else if (p.status && p.status.type === "idle") tell("Stop", {{ session_id }});
        break;
      case "permission.asked":
        tell("Notification", {{ session_id, notification_type: "permission_prompt", message: "OpenCode needs your permission" + (p.permission ? ": " + p.permission : "") }});
        break;
      case "question.asked":
        tell("Notification", {{ session_id, notification_type: "question", message: "OpenCode has a question for you" }});
        break;
    }}
  }},
}});
"#
    )
}

impl Adapter for OpenCode {
    fn tried_live(&self) -> bool {
        false
    }

    /// Its sessions stay in its own database, which an archive leaves be.
    fn resumes(&self) -> bool {
        true
    }

    fn command(&self, profile: &Profile, start: &Start, unasked: bool, _: Option<&Hooks>) -> String {
        let cmd = profile.command.trim();
        let mut line = if cmd.is_empty() { "opencode" } else { cmd }.to_string();
        if unasked && !has_flag(&line, &["--auto", "--yolo", "--dangerously-skip-permissions"]) {
            line.push_str(" --auto");
        }
        if has_flag(&line, &["-s", "--session", "-c", "--continue", "--fork"]) {
            return line;
        }
        match start {
            Start::Resume(id) => format!("{line} --session {id}"),
            Start::Last => format!("{line} --continue"),
            Start::New(_) => line,
            Start::Fork { from, .. } => format!("{line} --session {from} --fork"),
        }
    }

    fn forks(&self) -> bool {
        true
    }

    fn env(&self, profile: &Profile, hooks: Option<&Hooks>) -> Vec<(String, String)> {
        if Self::keeps_users_config(profile) {
            return Vec::new();
        }
        let Some(file) = hooks.and_then(|h| h.opencode_plugin.as_ref()) else { return Vec::new() };
        let url = format!("file://{}", if cfg!(windows) { format!("/{}", file.display().to_string().replace('\\', "/")) } else { file.display().to_string() });
        vec![(CONTENT_ENV.into(), serde_json::json!({ "plugin": [url] }).to_string())]
    }

    fn events(&self) -> Events {
        Events::All
    }

    /// Asks before it changes a file or runs a command.
    fn aside_env(&self, profile: &Profile, hooks: Option<&Hooks>) -> Vec<(String, String)> {
        let mut env = self.env(profile, hooks);
        for (_, value) in env.iter_mut().filter(|(key, _)| key == CONTENT_ENV) {
            if let Ok(mut config) = serde_json::from_str::<serde_json::Value>(value) {
                config["permission"] = serde_json::json!({ "edit": "ask", "bash": "ask" });
                *value = config.to_string();
            }
        }
        env
    }

    fn one_shot(&self, profile: &Profile, ask: &Ask) -> Option<OneShot> {
        let program = program(&profile.command);
        let program = if program.is_empty() { "opencode" } else { program }.to_string();
        let mut args: Vec<String> = vec!["run".into()];
        if let Some(model) = flag_value(&profile.command, &["-m", "--model"]) {
            args.extend(["--model".to_string(), model.to_string()]);
        }
        if let Ask::Read(folders) = ask {
            if let Some(first) = folders.first() {
                args.extend(["--dir".to_string(), first.to_string_lossy().into_owned()]);
            }
        }
        args.push("Answer the request given on standard input.".into());
        Some(OneShot { program, args, env: profile.env_vars(), answer: Answer::Printed })
    }
}

impl OpenCode {
    /// The profile's own setting of the variable wins over the plugin.
    pub fn keeps_users_config(profile: &Profile) -> bool {
        profile.env.contains_key(CONTENT_ENV) || std::env::var_os(CONTENT_ENV).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(command: &str) -> Profile {
        Profile { id: "oc".into(), name: "OpenCode".into(), kind: Kind::OpenCode, command: command.into(), env: Default::default(), ..Default::default() }
    }

    #[test]
    fn starts_and_resumes() {
        let p = profile("opencode -m anthropic/claude-sonnet-4");
        assert_eq!(OpenCode.command(&p, &Start::New(None), false, None), "opencode -m anthropic/claude-sonnet-4");
        assert_eq!(OpenCode.command(&p, &Start::Resume("ses_1".into()), true, None), "opencode -m anthropic/claude-sonnet-4 --auto --session ses_1");
        assert_eq!(OpenCode.command(&p, &Start::Last, false, None), "opencode -m anthropic/claude-sonnet-4 --continue");
        assert_eq!(OpenCode.command(&profile("opencode -s ses_9"), &Start::Resume("ses_1".into()), false, None), "opencode -s ses_9");
    }

    #[test]
    fn the_plugin_is_added_not_written_into_the_users_config() {
        let hooks = Hooks { opencode_plugin: Some(PathBuf::from("/run/ideae/opencode-plugin.js")), ..Default::default() };
        let env = OpenCode.env(&profile("opencode"), Some(&hooks));
        if cfg!(unix) {
            assert_eq!(env, [(CONTENT_ENV.to_string(), r#"{"plugin":["file:///run/ideae/opencode-plugin.js"]}"#.to_string())]);
        }
        let source = plugin_source(Path::new("/opt/ide \"ae\""));
        assert!(source.contains(r#"const exe = "/opt/ide \"ae\"";"#), "{source}");
        assert!(source.contains("\"session.status\""));
    }

    #[test]
    fn one_question() {
        let shot = OpenCode.one_shot(&profile("opencode"), &Ask::Quick).unwrap();
        assert_eq!(shot.program, "opencode");
        assert_eq!(shot.args[0], "run");
        assert_eq!(shot.answer, Answer::Printed);
    }
}
