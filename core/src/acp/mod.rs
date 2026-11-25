//! An agent that speaks ACP, hosted by `ideae --acp-host` (or
//! `ideae-server --acp-host` on another machine): the conversation and its
//! journal, the agent's files and terminals, who answers its questions.

pub mod checkpoint;
pub mod host;
pub mod policy;
pub mod terminal;
pub mod thread;
pub mod wire;

/// A conversation kept under a name of before (`old`) goes on under its
/// name now (`new`): its journal, its log and its checkpoints, once, before
/// its host reads it. Nothing when it has one under its name now.
pub fn carry_journal(old: &std::path::Path, new: &std::path::Path, repo: &std::path::Path) {
    if old == new || new.exists() || !old.is_file() {
        return;
    }
    if let Some(dir) = new.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::rename(old, new).is_err() {
        return;
    }
    let (old_log, new_log) = (host::log_path(old), host::log_path(new));
    let _ = std::fs::rename(old_log.with_extension("log.old"), new_log.with_extension("log.old"));
    let _ = std::fs::rename(old_log, new_log);
    checkpoint::rename_thread(repo, &checkpoint::thread_name(Some(old)), &checkpoint::thread_name(Some(new)));
}

/// What a cost said in USD is (`$0.42`).
pub fn usd(cost: &str) -> Option<f64> {
    cost.trim().strip_prefix('$')?.parse().ok()
}

#[cfg(all(test, unix))]
mod tests {
    /// A conversation under its name of before goes on under its name now,
    /// its log and checkpoints with it; one under its name now is left be.
    #[test]
    fn a_journal_of_before_is_carried_to_its_name_now() {
        let dir = std::env::temp_dir().join(format!("ideae-carry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("acp")).unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap().status.success(), "git {args:?}");
        git(&["init", "-q"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "one"]);
        git(&["update-ref", "refs/ideae/checkpoints/wt-00aa/1", "HEAD"]);
        let (old, new) = (dir.join("acp/wt-00aa.jsonl"), dir.join("acp/wt-11bb.jsonl"));
        std::fs::write(&old, "said\n").unwrap();
        std::fs::write(super::host::log_path(&old), "log\n").unwrap();
        super::carry_journal(&old, &new, &dir);
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "said\n");
        assert!(!old.exists() && super::host::log_path(&new).is_file());
        let refs = String::from_utf8(std::process::Command::new("git").args(["for-each-ref", "--format=%(refname)", "refs/ideae"]).current_dir(&dir).output().unwrap().stdout).unwrap();
        assert_eq!(refs.trim(), "refs/ideae/checkpoints/wt-11bb/1");
        // Once there, it is not replaced by another of before.
        std::fs::write(&old, "older\n").unwrap();
        super::carry_journal(&old, &new, &dir);
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "said\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
