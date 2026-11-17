//! OpenAI's Codex CLI, from its source (github.com/openai/codex, codex-rs/),
//! not tried live:
//! - `notify` (config_toml.rs, hooks/src/legacy_notify.rs) runs a program at
//!   the end of each turn with a JSON argument last: `{"type":
//!   "agent-turn-complete","thread-id","turn-id","cwd","last-assistant-message"}`.
//!   It is set for the session with `-c notify=[…]`; nothing of the user's
//!   config is touched. Its hooks need `--dangerously-bypass-hook-trust`, which
//!   would run the user's untrusted hooks too: not used.
//! - The TUI tells of approvals as terminal notifications; `tui.notification_method
//!   = "osc9"` and `tui.notification_condition = "always"` make them OSC 9.
//! - No session id can be chosen; `codex resume <id>` resumes one (cli/src/main.rs).
//! - Sessions: `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-…-<id>.jsonl`, with
//!   `event_msg` lines of `type: token_count` (protocol.rs TokenUsage).
//! - `codex exec -` reads the question from stdin and prints only the final
//!   message on stdout (exec/src/cli.rs).

use std::path::{Path, PathBuf};

use super::*;

pub struct Codex;

/// Where Codex keeps its sessions for this profile.
pub fn home(profile: &Profile) -> Option<PathBuf> {
    profile.var("CODEX_HOME").map(PathBuf::from).or_else(|| std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| PathBuf::from(h).join(".codex")))
}

/// `-c notify=[…]` running `ideae --hook Codex` with the task and the socket
/// as arguments: the shell puts in their values, and the program Codex
/// runs need not inherit them (its TUI may hand turns to a shared server).
fn notify_override(exe: &Path) -> Option<String> {
    let exe = exe.display().to_string();
    let exe = if cfg!(windows) { exe.replace('\\', "/") } else { exe };
    // TOML literal strings: no escapes, no single quote inside.
    if exe.contains('\'') || exe.contains('"') || exe.contains('$') || exe.contains('`') {
        return None;
    }
    let var = |name: &str| if cfg!(windows) { format!("$env:{name}") } else { format!("${name}") };
    Some(format!(
        "-c \"notify=['{exe}','--hook','Codex','--task={}','--sock={}']\"",
        var(crate::agent::TASK_ENV),
        var(crate::agent::SOCK_ENV)
    ))
}

impl Adapter for Codex {
    fn tried_live(&self) -> bool {
        false
    }

    fn command(&self, profile: &Profile, start: &Start, unasked: bool, hooks: Option<&Hooks>) -> String {
        let cmd = profile.command.trim();
        let cmd = if cmd.is_empty() { "codex" } else { cmd };
        let (program, rest) = cmd.split_once(char::is_whitespace).map_or((cmd, ""), |(p, r)| (p, r.trim()));
        let words: Vec<&str> = rest.split_whitespace().collect();
        // The user's own choice of session stands.
        let picks = matches!(words.first(), Some(&"resume") | Some(&"fork"));
        let mut line = program.to_string();
        if !picks {
            if let Start::Resume(id) = start {
                line.push_str(&format!(" resume {id}"));
            }
        }
        if !rest.is_empty() {
            line.push(' ');
            line.push_str(rest);
        }
        if unasked && !has_flag(cmd, &["-s", "--sandbox", "-a", "--ask-for-approval", "--yolo", "--dangerously-bypass-approvals-and-sandbox", "--approve-for-me", "--full-auto"]) {
            line.push_str(" --sandbox workspace-write --ask-for-approval never");
        }
        if let Some(notify) = hooks.and_then(|h| h.exe.as_deref()).and_then(notify_override) {
            line.push(' ');
            line.push_str(&notify);
            line.push_str(" -c tui.notification_method=\"osc9\" -c tui.notification_condition=\"always\"");
        }
        line
    }

    fn events(&self) -> Events {
        Events::TurnEnd
    }

    fn tells_usage(&self) -> bool {
        true
    }

    fn transcript(&self, profile: &Profile, _root: &Path, session: Option<&str>) -> Option<PathBuf> {
        let id = session?;
        let sessions = home(profile)?.join("sessions");
        find_rollout(&sessions, id)
    }

    fn usage(&self, transcript: &Path) -> Option<crate::usage::Usage> {
        rollout_usage(&std::fs::read_to_string(transcript).ok()?)
    }

    fn one_shot(&self, profile: &Profile, ask: &Ask) -> Option<OneShot> {
        let program = program(&profile.command);
        let program = if program.is_empty() { "codex" } else { program }.to_string();
        let mut args: Vec<String> = ["exec", "--skip-git-repo-check", "--sandbox", "read-only", "--color", "never"].map(String::from).to_vec();
        if let Some(model) = flag_value(&profile.command, &["-m", "--model"]) {
            args.extend(["--model".to_string(), model.to_string()]);
        }
        if let Ask::Read(folders) = ask {
            if let Some(first) = folders.first() {
                args.extend(["--cd".to_string(), first.to_string_lossy().into_owned()]);
            }
        }
        args.push("-".into());
        Some(OneShot { program, args, env: profile.env_vars(), answer: Answer::Printed })
    }
}

/// The rollout file of a session, by the id at the end of its name. The
/// folders are by day; once found it is remembered.
fn find_rollout(sessions: &Path, id: &str) -> Option<PathBuf> {
    use std::{collections::HashMap, sync::Mutex};
    static FOUND: Mutex<Option<HashMap<String, PathBuf>>> = Mutex::new(None);
    if let Some(path) = FOUND.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|found| found.get(id)).filter(|p| p.is_file()) {
        return Some(path.clone());
    }
    let suffix = format!("{id}.jsonl");
    let mut dirs = vec![sessions.to_path_buf()];
    let mut found = None;
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                dirs.push(path);
            } else if name.starts_with("rollout-") && name.ends_with(&suffix) {
                found = Some(path);
            }
        }
        if found.is_some() {
            break;
        }
    }
    let found = found?;
    let mut known = FOUND.lock().unwrap_or_else(|e| e.into_inner());
    let known = known.get_or_insert_with(HashMap::new);
    if known.len() > 256 {
        known.clear();
    }
    known.insert(id.to_string(), found.clone());
    Some(found)
}

/// What a session used, from its `token_count` events: the totals of the
/// last, the context of the latest turn, and what the last day took.
pub fn rollout_usage(text: &str) -> Option<crate::usage::Usage> {
    let mut usage = crate::usage::Usage::default();
    let day_ago = crate::state::now().saturating_sub(86_400);
    let mut before = 0u64;
    let mut seen = false;
    for line in text.lines() {
        if !line.contains("token_count") && !line.contains("turn_context") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let payload = &v["payload"];
        if v["type"] == "turn_context" {
            if let Some(model) = payload["model"].as_str() {
                usage.model = model.to_string();
            }
            continue;
        }
        if v["type"] != "event_msg" || payload["type"] != "token_count" || !payload["info"].is_object() {
            continue;
        }
        seen = true;
        let info = &payload["info"];
        let n = |u: &serde_json::Value, key: &str| u[key].as_u64().unwrap_or(0);
        let total = &info["total_token_usage"];
        let cached = n(total, "cached_input_tokens");
        usage.input = n(total, "input_tokens").saturating_sub(cached);
        usage.cache_read = cached;
        usage.output = n(total, "output_tokens");
        let spent = usage.input + usage.output;
        let at = v["timestamp"].as_str().and_then(crate::usage::epoch).unwrap_or(0);
        if at >= day_ago {
            usage.recent += spent.saturating_sub(before);
        }
        before = spent;
        usage.answers += 1;
        usage.context = n(&info["last_token_usage"], "input_tokens");
        if let Some(window) = info["model_context_window"].as_u64() {
            usage.window = window;
        }
    }
    seen.then_some(usage)
}

/// What `ideae --hook Codex <json>` hands on: a turn's end, as Claude
/// Code's Stop would say it.
pub fn notify_payload(json: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    (v["type"] == "agent-turn-complete").then(|| {
        serde_json::json!({
            "session_id": v["thread-id"],
            "last_assistant_message": v["last-assistant-message"],
            "cwd": v["cwd"],
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(command: &str) -> Profile {
        Profile { id: "codex".into(), name: "Codex".into(), kind: Kind::Codex, command: command.into(), env: Default::default() }
    }

    #[test]
    fn starts_and_resumes() {
        let p = profile("codex -m o3");
        assert_eq!(Codex.command(&p, &Start::New(None), false, None), "codex -m o3");
        assert_eq!(Codex.command(&p, &Start::Resume("0199-a".into()), false, None), "codex resume 0199-a -m o3");
        assert_eq!(Codex.command(&p, &Start::Last, true, None), "codex -m o3 --sandbox workspace-write --ask-for-approval never");
        assert_eq!(Codex.command(&profile("codex resume --last"), &Start::Resume("x".into()), false, None), "codex resume --last");
        assert_eq!(Codex.command(&profile("codex --yolo"), &Start::New(None), true, None), "codex --yolo");
        let hooks = Hooks { exe: Some(PathBuf::from("/opt/ideae")), ..Default::default() };
        let with = Codex.command(&p, &Start::New(None), false, Some(&hooks));
        if cfg!(unix) {
            assert!(with.contains(r#"-c "notify=['/opt/ideae','--hook','Codex','--task=$IDEAE_TASK','--sock=$IDEAE_SOCK']""#), "{with}");
        }
        assert!(with.contains("tui.notification_method=\"osc9\""), "{with}");
    }

    #[test]
    fn one_question() {
        let shot = Codex.one_shot(&profile("codex -m o3 --yolo"), &Ask::Read(vec!["/tmp/j".into()])).unwrap();
        assert_eq!(shot.args, ["exec", "--skip-git-repo-check", "--sandbox", "read-only", "--color", "never", "--model", "o3", "--cd", "/tmp/j", "-"]);
        assert_eq!(shot.read_answer("  Keep A\n"), ("Keep A".to_string(), 0));
    }

    #[test]
    fn notify_is_a_turns_end() {
        let json = r#"{"type":"agent-turn-complete","thread-id":"0199a","turn-id":"1","cwd":"/w","input-messages":["hi"],"last-assistant-message":"Done: fixed it."}"#;
        let v = notify_payload(json).unwrap();
        assert_eq!((v["session_id"].as_str(), v["last_assistant_message"].as_str()), (Some("0199a"), Some("Done: fixed it.")));
        assert!(notify_payload(r#"{"type":"other"}"#).is_none());
    }

    #[test]
    fn usage_of_a_rollout() {
        let now = crate::usage::stamp(crate::state::now());
        let old = crate::usage::stamp(crate::state::now() - 3 * 86_400);
        let count = |at: &str, input: u64, cached: u64, output: u64, last: u64| {
            format!(r#"{{"timestamp":"{at}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":0}},"last_token_usage":{{"input_tokens":{last},"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":0}},"model_context_window":272000}},"rate_limits":null}}}}"#)
        };
        let text = [
            r#"{"timestamp":"x","type":"session_meta","payload":{"id":"0199a","cwd":"/w"}}"#.to_string(),
            r#"{"timestamp":"x","type":"turn_context","payload":{"model":"gpt-5-codex"}}"#.to_string(),
            count(&old, 1000, 0, 100, 1000),
            r#"{"timestamp":"x","type":"event_msg","payload":{"type":"token_count","info":null}}"#.to_string(),
            count(&now, 5000, 3000, 300, 4000),
        ]
        .join("\n");
        let u = rollout_usage(&text).unwrap();
        assert_eq!((u.input, u.cache_read, u.output, u.context, u.window, u.answers), (2000, 3000, 300, 4000, 272_000, 2));
        assert_eq!(u.recent, 2300 - 1100, "the day's share");
        assert_eq!(u.model, "gpt-5-codex");
        assert!((u.fill() - 4000. / 272_000.).abs() < 1e-6);
        assert!(rollout_usage("{}").is_none());
    }

    #[test]
    fn rollouts_by_id() {
        let dir = std::env::temp_dir().join(format!("ideae-codex-{}", std::process::id()));
        let day = dir.join("sessions/2026/11/17");
        std::fs::create_dir_all(&day).unwrap();
        let file = day.join("rollout-2026-11-17T10-00-00-0199aaaa-bbbb.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        let mut p = profile("codex");
        p.env.insert("CODEX_HOME".into(), dir.display().to_string());
        assert_eq!(Codex.transcript(&p, Path::new("/w"), Some("0199aaaa-bbbb")), Some(file));
        assert_eq!(Codex.transcript(&p, Path::new("/w"), Some("nope")), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
