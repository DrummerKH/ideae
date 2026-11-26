//! Google's Antigravity CLI (`agy`), from its documentation
//! (antigravity.google/docs/cli), not tried live:
//! - `--conversation <id>` resumes a conversation, `--continue` (`-c`) the
//!   latest of the folder; a new one's id cannot be chosen, and none is told
//!   the app but by its hooks: without them a task goes on with the latest
//!   of its worktree.
//! - `--mode auto` lets it act unasked inside its terminal sandbox;
//!   `--dangerously-skip-permissions` would allow everything.
//! - Its hooks are read from files alone (`~/.gemini/config/hooks.json`, or
//!   the workspace's `.agents/hooks.json`, which would be the project's):
//!   ideae's are put in the first when the user says so (own_config.rs), and
//!   then it tells its conversation id and a turn's end. It has no hook for
//!   a question: that, and without the hooks all of it, is guessed from its
//!   terminal.
//! - `agy -p` takes its prompt as an argument; on standard input only as
//!   stream-json: it answers no quick questions.
//! - `agy acp` is its ACP server, for a chat profile.

use super::*;

pub struct Antigravity;

impl Adapter for Antigravity {
    fn tried_live(&self) -> bool {
        false
    }

    fn events_hooked(&self) -> Events {
        Events::TurnEnd
    }

    fn command(&self, profile: &Profile, start: &Start, unasked: bool, _: Option<&Hooks>) -> String {
        let cmd = profile.command.trim();
        let mut line = if cmd.is_empty() { "agy" } else { cmd }.to_string();
        if unasked && !has_flag(&line, &["--mode", "--dangerously-skip-permissions"]) {
            line.push_str(" --mode auto");
        }
        if has_flag(&line, &["--conversation", "-r", "--resume", "-c", "--continue"]) {
            return line;
        }
        match start {
            Start::Resume(id) => or_else(&format!("{line} --conversation {id}"), &line),
            Start::Last => or_else(&format!("{line} --continue"), &line),
            Start::New(_) | Start::Fork { .. } => line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(command: &str) -> Profile {
        Profile { id: "agy".into(), name: "Antigravity".into(), kind: Kind::Antigravity, command: command.into(), ..Default::default() }
    }

    #[test]
    fn starts_and_goes_on() {
        assert_eq!(Antigravity.command(&profile("agy --model gemini-3-pro"), &Start::New(None), false, None), "agy --model gemini-3-pro");
        assert_eq!(Antigravity.command(&profile(""), &Start::New(None), true, None), "agy --mode auto");
        assert_eq!(Antigravity.command(&profile("agy --mode=plan"), &Start::New(None), true, None), "agy --mode=plan");
        if cfg!(unix) {
            assert_eq!(Antigravity.command(&profile("agy"), &Start::Last, false, None), "agy --continue || agy");
            assert_eq!(Antigravity.command(&profile("agy"), &Start::Resume("c1".into()), false, None), "agy --conversation c1 || agy");
        }
        assert_eq!(Antigravity.command(&profile("agy -c"), &Start::Resume("c1".into()), false, None), "agy -c");
        assert!(Antigravity.one_shot(&profile("agy"), &Ask::Quick).is_none());
    }
}
