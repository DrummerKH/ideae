//! Claude Code: hooks through a `--settings` file (agent.rs), session ids
//! the app chooses, transcripts in `~/.claude/projects` (or the profile's
//! `CLAUDE_CONFIG_DIR`), `claude -p` for one question.

use std::path::{Path, PathBuf};

use super::*;

pub struct Claude;

/// The user's own choice of conversation, which stands.
fn picks_conversation(command: &str) -> bool {
    has_flag(command, &["-c", "--continue", "-r", "--resume", "--session-id"])
}

/// Where Claude Code keeps its sessions for this profile.
pub fn config_dir(profile: &Profile) -> Option<PathBuf> {
    profile.var("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| PathBuf::from(h).join(".claude")))
}

/// Claude Code's folder name for a working directory: each character that
/// is not a letter or a digit becomes a dash.
pub fn project_dir(path: &Path) -> String {
    path.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// The answer and the tokens it took, of what `claude -p --output-format
/// json` prints; what is no such thing is the answer itself.
pub fn json_answer(printed: &str) -> (String, u64) {
    let Ok(told) = serde_json::from_str::<serde_json::Value>(printed.trim()) else { return (printed.trim().to_string(), 0) };
    let Some(answer) = told["result"].as_str() else { return (printed.trim().to_string(), 0) };
    let used = |key: &str| told["usage"][key].as_u64().unwrap_or(0);
    (answer.trim().to_string(), used("input_tokens") + used("cache_creation_input_tokens") + used("output_tokens"))
}

impl Adapter for Claude {
    fn chooses_session_id(&self) -> bool {
        true
    }

    fn command(&self, profile: &Profile, start: &Start, unasked: bool, hooks: Option<&Hooks>) -> String {
        let mut cmd = profile.command.trim().to_string();
        if cmd.is_empty() {
            cmd = "claude".into();
        }
        // Variants: the same question in every one of them is what nobody sits through.
        if unasked && !has_flag(&cmd, &["--permission-mode", "--dangerously-skip-permissions"]) {
            cmd.push_str(" --permission-mode auto");
        }
        if let Some(settings) = hooks.and_then(|h| h.claude_settings.as_ref()) {
            cmd = format!("{cmd} --settings {}", quote(&settings.display().to_string()));
        }
        if let Some(mcp) = hooks.and_then(|h| h.claude_mcp.as_ref()) {
            cmd = format!("{cmd} --mcp-config {}", quote(&mcp.display().to_string()));
        }
        if picks_conversation(&cmd) {
            return cmd;
        }
        match start {
            Start::New(Some(id)) => format!("{cmd} --session-id {id}"),
            Start::New(None) => cmd,
            // Never begun (it was quit before a first prompt): begun now, under its id.
            Start::Resume(id) => or_else(&format!("{cmd} --resume {id}"), &format!("{cmd} --session-id {id}")),
            Start::Last => or_else(&format!("{cmd} --continue"), &cmd),
        }
    }

    fn takes_mcp(&self, hooks: Option<&Hooks>) -> bool {
        hooks.is_some_and(|h| h.claude_mcp.is_some())
    }

    fn events(&self) -> Events {
        Events::All
    }

    fn reports_ready(&self) -> bool {
        true
    }

    fn tells_usage(&self) -> bool {
        true
    }

    fn transcript(&self, profile: &Profile, root: &Path, session: Option<&str>) -> Option<PathBuf> {
        let dir = config_dir(profile)?.join("projects").join(project_dir(root));
        if let Some(id) = session {
            let file = dir.join(format!("{id}.jsonl"));
            if file.is_file() {
                return Some(file);
            }
        }
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .max_by_key(|(at, _)| *at)
            .map(|(_, path)| path)
    }

    fn usage(&self, transcript: &Path) -> Option<crate::usage::Usage> {
        crate::usage::read(transcript)
    }

    fn last_answer(&self, transcript: &Path) -> Option<String> {
        crate::usage::last_answer(transcript)
    }

    fn resumes(&self) -> bool {
        true
    }

    /// Its projects folder for the working directory: the transcript is
    /// `<id>.jsonl` there, its subagents' and tools' files in `<id>/`.
    fn session_base(&self, profile: &Profile, root: &Path) -> Option<PathBuf> {
        Some(config_dir(profile)?.join("projects").join(project_dir(root)))
    }

    fn session_files(&self, profile: &Profile, root: &Path, session: Option<&str>) -> Option<SessionFiles> {
        let base = self.session_base(profile, root)?;
        let transcript = self.transcript(profile, root, session)?;
        let id = transcript.file_stem()?.to_string_lossy().into_owned();
        let mut files = vec![PathBuf::from(format!("{id}.jsonl"))];
        files.extend(files_under(&base, &base.join(&id)));
        Some(SessionFiles { base, id: Some(id), files })
    }

    fn one_shot(&self, profile: &Profile, ask: &Ask) -> Option<OneShot> {
        let program = program(&profile.command);
        let program = if program.is_empty() { "claude" } else { program }.to_string();
        let mut args: Vec<String> = vec!["-p".into()];
        let answer = match ask {
            // Quick and on its own: no tools, no MCP servers, no session kept.
            Ask::Quick => {
                args.extend(["--model", "haiku", "--tools", "", "--strict-mcp-config", "--no-session-persistence"].map(String::from));
                Answer::Printed
            }
            // Of the agent command only the program and its model are
            // taken: what lets it act without asking, or go on with an
            // earlier talk, is not for a judge. As JSON: with the answer
            // comes what it took.
            Ask::Read(folders) => {
                args.extend(["--no-session-persistence", "--output-format", "json", "--tools", "Read,Grep,Glob"].map(String::from));
                if let Some(model) = flag_value(&profile.command, &["--model"]) {
                    args.extend(["--model".to_string(), model.to_string()]);
                }
                for folder in folders {
                    args.extend(["--add-dir".to_string(), folder.to_string_lossy().into_owned()]);
                }
                Answer::ClaudeJson
            }
        };
        Some(OneShot { program, args, env: profile.env_vars(), answer })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks() -> Hooks {
        Hooks { claude_settings: Some(PathBuf::from("/run/h.json")), ..Default::default() }
    }

    fn command(cmd: &str, start: Start, unasked: bool) -> String {
        let profile = Profile { command: cmd.into(), ..Profile::claude_code() };
        Claude.command(&profile, &start, unasked, Some(&hooks()))
    }

    #[test]
    fn sessions_by_id() {
        assert_eq!(command("claude", Start::New(Some("u-1".into())), false), "claude --settings '/run/h.json' --session-id u-1");
        let resumed = command("claude --model opus", Start::Resume("u-1".into()), false);
        assert!(resumed.starts_with("claude --model opus --settings '/run/h.json' --resume u-1"), "{resumed}");
        assert!(resumed.ends_with("claude --model opus --settings '/run/h.json' --session-id u-1 }") || resumed.ends_with("--settings '/run/h.json' --session-id u-1"), "{resumed}");
        // A task of before: the latest conversation in its folder, else a new one.
        let last = command("claude --model opus", Start::Last, false);
        assert!(last.starts_with("claude --model opus --settings '/run/h.json' --continue"), "{last}");
        assert_eq!(last.matches("claude --model opus --settings '/run/h.json'").count(), 2, "{last}");
        // The user's own choice stands.
        assert_eq!(command("claude --resume abc", Start::New(Some("u".into())), false), "claude --resume abc --settings '/run/h.json'");
    }

    #[test]
    fn it_is_given_ideae_s_mcp_server_by_its_terminal() {
        let given = Hooks { claude_mcp: Some(PathBuf::from("/run/h.mcp.json")), ..hooks() };
        let profile = Profile { command: "claude".into(), ..Default::default() };
        assert_eq!(
            Claude.command(&profile, &Start::New(Some("u".into())), false, Some(&given)),
            "claude --settings '/run/h.json' --mcp-config '/run/h.mcp.json' --session-id u"
        );
        assert!(Claude.takes_mcp(Some(&given)) && !Claude.takes_mcp(Some(&hooks())));
    }

    #[test]
    fn unasked_is_auto_mode_unless_chosen() {
        assert_eq!(command("claude --model opus", Start::New(None), true), "claude --model opus --permission-mode auto --settings '/run/h.json'");
        assert_eq!(command("claude --permission-mode plan", Start::New(None), true), "claude --permission-mode plan --settings '/run/h.json'");
        let profile = Profile::claude_code();
        assert_eq!(Claude.command(&profile, &Start::New(None), false, None), "claude");
    }

    #[test]
    fn one_question() {
        let profile = Profile { command: "/usr/local/bin/claude --continue --model opus --permission-mode bypass".into(), ..Profile::claude_code() };
        let judge = Claude.one_shot(&profile, &Ask::Read(vec!["/w/fix a".into()])).unwrap();
        if cfg!(unix) {
            assert_eq!(judge.line(), "'/usr/local/bin/claude' '-p' '--no-session-persistence' '--output-format' 'json' '--tools' 'Read,Grep,Glob' '--model' 'opus' '--add-dir' '/w/fix a'");
        }
        assert_eq!(judge.read_answer(r#"{"result":" Keep B. ","usage":{"input_tokens":10,"cache_creation_input_tokens":5,"output_tokens":3}}"#), ("Keep B.".to_string(), 18));
        let quick = Claude.one_shot(&Profile::claude_code(), &Ask::Quick).unwrap();
        assert_eq!(quick.args, ["-p", "--model", "haiku", "--tools", "", "--strict-mcp-config", "--no-session-persistence"]);
    }

    #[test]
    fn transcripts_by_session() {
        let dir = std::env::temp_dir().join(format!("ideae-claude-tr-{}", std::process::id()));
        let root = Path::new("/w/repo.worktrees/fix");
        let projects = dir.join("projects").join(project_dir(root));
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(projects.join("aaa.jsonl"), "{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(projects.join("bbb.jsonl"), "{}\n").unwrap();
        let mut profile = Profile::claude_code();
        profile.env.insert("CLAUDE_CONFIG_DIR".into(), dir.display().to_string());
        assert_eq!(Claude.transcript(&profile, root, Some("aaa")), Some(projects.join("aaa.jsonl")));
        assert_eq!(Claude.transcript(&profile, root, None), Some(projects.join("bbb.jsonl")), "the latest");
        let _ = std::fs::remove_dir_all(dir);
    }
}
