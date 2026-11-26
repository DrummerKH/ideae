//! Z.ai's GLM models through Claude Code (`claude` with `ANTHROPIC_BASE_URL`
//! at Z.ai's Anthropic endpoint and a GLM Coding Plan key in
//! `ANTHROPIC_AUTH_TOKEN`), from Z.ai's documentation (docs.z.ai/devpack).
//!
//! It overlaps claude.rs nearly everywhere today: the program, its flags,
//! hooks, session ids, transcripts. It is kept apart on purpose: what runs
//! behind the endpoint is not Anthropic's, and where the two part (models
//! named for Claude and mapped by Z.ai, what the endpoint accepts of a
//! session made elsewhere, context windows, prices) is decided here, never
//! by a branch in claude.rs. Only claude.rs's helpers for where Claude Code
//! keeps its files (`config_dir`, `project_dir`) are shared.
//!
//! Known differences:
//! - A session is not carried over to or from Anthropic's models: their
//!   signed thinking is refused by the other endpoint (`shares_sessions`).
//! - Cost reported is Anthropic's prices: a chat on GLM has no daily limit.
//! - The context window is guessed as Claude's (usage.rs).

use std::path::{Path, PathBuf};

use super::*;
use super::claude::{config_dir, project_dir};

pub struct Glm;

/// The user's own choice of conversation, which stands.
fn picks_conversation(command: &str) -> bool {
    has_flag(command, &["-c", "--continue", "-r", "--resume", "--session-id"])
}

impl Adapter for Glm {
    fn tried_live(&self) -> bool {
        false
    }

    fn chooses_session_id(&self) -> bool {
        true
    }

    fn command(&self, profile: &Profile, start: &Start, unasked: bool, hooks: Option<&Hooks>) -> String {
        let mut cmd = profile.command.trim().to_string();
        if cmd.is_empty() {
            cmd = "claude".into();
        }
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
            Start::Resume(id) => or_else(&format!("{cmd} --resume {id}"), &format!("{cmd} --session-id {id}")),
            Start::Last => or_else(&format!("{cmd} --continue"), &cmd),
            Start::Fork { from, id: Some(id) } => format!("{cmd} --resume {from} --fork-session --session-id {id}"),
            Start::Fork { from, id: None } => format!("{cmd} --resume {from} --fork-session"),
        }
    }

    fn forks(&self) -> bool {
        true
    }

    fn takes_mcp(&self, hooks: Option<&Hooks>) -> bool {
        hooks.is_some_and(|h| h.claude_mcp.is_some())
    }

    fn aside(&self, command: &str) -> String {
        let cmd = if command.trim().is_empty() { "claude" } else { command.trim() };
        format!("{cmd} --append-system-prompt {}", quote(super::ASIDE))
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

    /// `haiku` is what Z.ai maps to its small model, unless the profile's
    /// `ANTHROPIC_DEFAULT_HAIKU_MODEL` says another.
    fn one_shot(&self, profile: &Profile, ask: &Ask) -> Option<OneShot> {
        let program = program(&profile.command);
        let program = if program.is_empty() { "claude" } else { program }.to_string();
        let mut args: Vec<String> = vec!["-p".into()];
        let answer = match ask {
            Ask::Quick => {
                args.extend(["--model", "haiku", "--tools", "", "--strict-mcp-config", "--no-session-persistence"].map(String::from));
                Answer::Printed
            }
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

    fn profile(command: &str) -> Profile {
        let env = Kind::Glm.preset_env().iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Profile { id: "glm".into(), name: "Z.ai GLM".into(), kind: Kind::Glm, command: command.into(), env, ..Default::default() }
    }

    #[test]
    fn claude_code_pointed_at_z_ai() {
        let hooks = Hooks { claude_settings: Some(PathBuf::from("/run/h.json")), ..Default::default() };
        let p = profile("claude");
        assert_eq!(Glm.command(&p, &Start::New(Some("u-1".into())), false, Some(&hooks)), "claude --settings '/run/h.json' --session-id u-1");
        assert_eq!(Glm.command(&p, &Start::New(None), true, None), "claude --permission-mode auto");
        assert!(p.env_vars().contains(&("ANTHROPIC_BASE_URL".to_string(), GLM_BASE_URL.to_string())));
        let shot = Glm.one_shot(&p, &Ask::Quick).unwrap();
        assert_eq!((shot.program.as_str(), shot.args[0].as_str()), ("claude", "-p"));
        assert!(shot.env.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"), "the question goes to Z.ai too");
        assert_eq!(Kind::Glm.adapter().events(), Events::All);
    }
}
