//! Moonshot's Kimi Code CLI (`kimi`), from its documentation
//! (moonshotai.github.io/kimi-code, the `kimi` command), not tried live:
//! - `--session <id>` (`-S`) resumes a session, `--continue` the latest of
//!   the folder (`-c` in Kimi Code, `-C` in the kimi-cli of before, where
//!   `-c` was a command: only the long flag is written or taken as one); a new session's id cannot be chosen, and none is
//!   told the app but by its hooks: without them a task goes on with the
//!   latest of its worktree.
//! - `--yolo` runs edits and commands unasked, still asking for what is
//!   risky; `--auto` would ask nothing at all.
//! - Its hooks are read from its own config.toml alone: ideae's are put
//!   there when the user says so (own_config.rs), and then it tells when it
//!   works, asks and is done, and its session id. Without them what it does
//!   is guessed from its terminal.
//! - `kimi -p` takes its prompt as an argument, not on standard input: it
//!   answers no quick questions.
//! - `kimi acp` is its ACP server, for a chat profile.

use super::*;

pub struct Kimi;

impl Adapter for Kimi {
    fn tried_live(&self) -> bool {
        false
    }

    fn events_hooked(&self) -> Events {
        Events::All
    }

    fn command(&self, profile: &Profile, start: &Start, unasked: bool, _: Option<&Hooks>) -> String {
        let cmd = profile.command.trim();
        let mut line = if cmd.is_empty() { "kimi" } else { cmd }.to_string();
        if unasked && !has_flag(&line, &["-y", "--yolo", "--yes", "--auto-approve", "--auto", "--plan"]) {
            line.push_str(" --yolo");
        }
        if has_flag(&line, &["-S", "--session", "-r", "--resume", "-C", "--continue"]) {
            return line;
        }
        match start {
            // A session gone (its folder cleaned): a new one rather than none.
            Start::Resume(id) => or_else(&format!("{line} --session {id}"), &line),
            Start::Last => or_else(&format!("{line} --continue"), &line),
            Start::New(_) | Start::Fork { .. } => line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(command: &str) -> Profile {
        Profile { id: "kimi".into(), name: "Kimi Code".into(), kind: Kind::Kimi, command: command.into(), ..Default::default() }
    }

    #[test]
    fn starts_and_goes_on() {
        let p = profile("kimi -m kimi-code/kimi-for-coding");
        assert_eq!(Kimi.command(&p, &Start::New(None), false, None), "kimi -m kimi-code/kimi-for-coding");
        assert_eq!(Kimi.command(&profile(""), &Start::New(None), true, None), "kimi --yolo");
        assert_eq!(Kimi.command(&profile("kimi --auto"), &Start::New(None), true, None), "kimi --auto", "the user's own mode stands");
        if cfg!(unix) {
            assert_eq!(Kimi.command(&profile("kimi"), &Start::Last, false, None), "kimi --continue || kimi");
            assert_eq!(Kimi.command(&profile("kimi"), &Start::Resume("s1".into()), true, None), "kimi --yolo --session s1 || kimi --yolo");
        }
        assert_eq!(Kimi.command(&profile("kimi -S mine"), &Start::Last, false, None), "kimi -S mine");
        assert!(Kimi.one_shot(&p, &Ask::Quick).is_none());
        assert_eq!(Kimi.events(), Events::None);
    }
}
