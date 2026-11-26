//! ideae's hooks in an agent's own configuration, for the agents that read
//! hooks from there alone (Kimi Code's `config.toml`, Antigravity's
//! `hooks.json`). Written only when the user says so, taken out the same
//! way; nothing else of the file changes, and a copy of it as it was is
//! kept beside it the first time.
//!
//! The hooks run a small script of the app's (`agent-hook` in its data
//! folder), written again at each start with where the app is now: the
//! agent's file names the script, never a build. Outside a task of this
//! app (no socket of its own in the agent's variables) the script ends at
//! once, saying nothing.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::RwLock,
};

use super::{Kind, Profile};

/// What each agent's hooks run, by its event: the event ideae is told.
const KIMI_EVENTS: [&str; 7] = ["SessionStart", "UserPromptSubmit", "PermissionRequest", "PermissionResult", "Stop", "StopFailure", "Interrupt"];
/// Antigravity tells no question: those are still read from its terminal.
const ANTIGRAVITY_EVENTS: [&str; 3] = ["SessionStart", "PreInvocation", "Stop"];

/// Whether a configuration file has ideae's hooks, as read last.
static HOOKED: RwLock<Option<HashMap<PathBuf, bool>>> = RwLock::new(None);

/// The agents this is for.
pub fn takes_hooks(kind: Kind) -> bool {
    cfg!(unix) && matches!(kind, Kind::Kimi | Kind::Antigravity)
}

/// What the offer and the profile editor say the hooks give.
pub fn gives(kind: Kind) -> &'static str {
    match kind {
        Kind::Kimi => "Its task then shows when it works, asks for permission and is done, as it says, not guessed; and a restart resumes the task's own session.",
        _ => "Its task then shows when a turn ends, as it says; and a restart resumes the task's own conversation. Its questions are still read from its terminal.",
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// The file a terminal profile of this machine reads its hooks from.
pub fn config_file(profile: &Profile) -> Option<PathBuf> {
    if profile.acp || !takes_hooks(profile.kind) {
        return None;
    }
    match profile.kind {
        Kind::Kimi => Some(profile.var("KIMI_CODE_HOME").map(PathBuf::from).or_else(|| Some(home()?.join(".kimi-code")))?.join("config.toml")),
        Kind::Antigravity => Some(home()?.join(".gemini/config/hooks.json")),
        _ => None,
    }
}

/// The file as one says it: the home folder as `~`.
pub fn shown(path: &Path) -> String {
    match home().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Whether the profile's agent has ideae's hooks, as last read: none when
/// not read yet or not one of these. No I/O.
pub fn hooked(profile: &Profile) -> Option<bool> {
    let file = config_file(profile)?;
    HOOKED.read().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(&file).copied()
}

fn note(file: &Path, on: bool) {
    HOOKED.write().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(file.to_path_buf(), on);
}

/// The script the hooks run.
pub fn script() -> PathBuf {
    crate::state::data_dir().join("agent-hook")
}

/// What the hook of `event` runs.
fn command(kind: Kind, event: &str) -> String {
    let prefix = if kind == Kind::Kimi { "Kimi" } else { "Antigravity" };
    format!("{} {prefix}:{event}", super::quote(&script().display().to_string()))
}

fn is_ours(command: &str) -> bool {
    command.contains(&script().display().to_string())
}

/// Writes the script for this build: at the start, and before hooks are
/// added. Reports go only to a socket of this app (a release and a debug
/// build each have their own script and hooks).
pub fn write_script(exe: &Path) -> std::io::Result<()> {
    let path = script();
    let runtime = crate::state::runtime_dir();
    let text = format!(
        "#!/bin/sh\n\
         # Written by ideae at each start: tells it what an agent of its tasks does.\n\
         # Outside them it does nothing.\n\
         case \"$1\" in Antigravity:*) echo '{{}}' ;; esac\n\
         case \"$IDEAE_SOCK\" in {runtime}/*) ;; *) exit 0 ;; esac\n\
         [ -x {exe} ] || exit 0\n\
         exec {exe} --hook \"$@\" >/dev/null 2>&1\n",
        runtime = super::quote(&runtime.display().to_string()),
        exe = super::quote(&exe.display().to_string()),
    );
    if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &path)
}

/// Reads whether each of these profiles' files has the hooks. Not for the
/// UI thread.
pub fn refresh(profiles: &[Profile]) {
    for profile in profiles {
        if let Some(file) = config_file(profile) {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            note(&file, has_hooks(profile.kind, &text));
        }
    }
}

fn has_hooks(kind: Kind, text: &str) -> bool {
    match kind {
        Kind::Kimi => text.parse::<toml_edit::DocumentMut>().ok().and_then(|doc| doc.get("hooks").and_then(|h| h.as_array_of_tables()).map(|hooks| hooks.iter().any(|t| t.get("command").and_then(|c| c.as_str()).is_some_and(is_ours)))).unwrap_or(false),
        Kind::Antigravity => serde_json::from_str::<serde_json::Value>(text).ok().is_some_and(|v| v.get(crate::state::APP_ID).is_some()),
        _ => false,
    }
}

/// `text` with ideae's hooks, or without them.
fn edited(kind: Kind, text: &str, on: bool) -> Result<String, String> {
    match kind {
        Kind::Kimi => {
            let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| format!("it is not TOML as read here: {e}"))?;
            if doc.get("hooks").is_some_and(|h| h.as_array_of_tables().is_none()) {
                return Err("its hooks are written in a way ideae does not change (not [[hooks]] tables): add them by hand".into());
            }
            if !doc.contains_key("hooks") {
                doc.insert("hooks", toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
            }
            let hooks = doc["hooks"].as_array_of_tables_mut().ok_or("no hooks")?;
            hooks.retain(|t| !t.get("command").and_then(|c| c.as_str()).is_some_and(is_ours));
            if on {
                for (ix, event) in KIMI_EVENTS.iter().enumerate() {
                    let mut table = toml_edit::Table::new();
                    table["event"] = toml_edit::value(*event);
                    table["command"] = toml_edit::value(command(Kind::Kimi, event));
                    table["timeout"] = toml_edit::value(5);
                    if ix == 0 {
                        table.decor_mut().set_prefix("\n# Added by ideae when asked: tells it what Kimi Code does in its tasks; nothing outside them.\n");
                    }
                    hooks.push(table);
                }
            }
            if hooks.is_empty() {
                doc.remove("hooks");
            }
            Ok(doc.to_string())
        }
        Kind::Antigravity => {
            let mut value: serde_json::Value = if text.trim().is_empty() { serde_json::json!({}) } else { serde_json::from_str(text).map_err(|e| format!("it is not JSON as read here: {e}"))? };
            let map = value.as_object_mut().ok_or("it is not an object of hooks")?;
            map.remove(crate::state::APP_ID);
            if on {
                let events: serde_json::Map<String, serde_json::Value> =
                    ANTIGRAVITY_EVENTS.iter().map(|e| (e.to_string(), serde_json::json!([{ "type": "command", "command": command(Kind::Antigravity, e), "timeout": 5 }]))).collect();
                map.insert(crate::state::APP_ID.to_string(), serde_json::Value::Object(events));
            }
            Ok(format!("{}\n", serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?))
        }
        _ => Err("this agent takes no hooks of ideae's".into()),
    }
}

/// Adds ideae's hooks to the profile's file (`on`), or takes them out.
/// Not for the UI thread.
pub fn set(profile: &Profile, on: bool) -> Result<(), String> {
    let file = config_file(profile).ok_or("this agent takes no hooks of ideae's")?;
    let say = |e: String| format!("{}: {e}", shown(&file));
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        write_script(&exe).map_err(|e| say(e.to_string()))?;
    }
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(say(e.to_string())),
    };
    let new = edited(profile.kind, &text, on).map_err(say)?;
    if new != text {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| say(e.to_string()))?;
        }
        // The file as it was before ideae first changed it.
        let backup = file.with_extension(format!("{}.before-ideae", file.extension().and_then(|e| e.to_str()).unwrap_or("conf")));
        if !text.is_empty() && !backup.exists() {
            let _ = std::fs::write(&backup, &text);
        }
        let tmp = file.with_extension("ideae-tmp");
        std::fs::write(&tmp, &new).map_err(|e| say(e.to_string()))?;
        std::fs::rename(&tmp, &file).map_err(|e| say(e.to_string()))?;
    }
    note(&file, on);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_s_config_keeps_what_it_had() {
        let before = "# mine\ndefault_model = \"k2\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"notify-send done\"\n";
        let with = edited(Kind::Kimi, before, true).unwrap();
        assert!(with.starts_with(before), "{with}");
        assert!(has_hooks(Kind::Kimi, &with));
        assert_eq!(with.matches("[[hooks]]").count(), 1 + KIMI_EVENTS.len());
        assert!(with.contains("Kimi:PermissionRequest"));
        // Again: not twice.
        assert_eq!(edited(Kind::Kimi, &with, true).unwrap(), with);
        let without = edited(Kind::Kimi, &with, false).unwrap();
        assert!(!has_hooks(Kind::Kimi, &without));
        assert!(without.contains("notify-send done") && without.contains("default_model"), "{without}");
        assert!(edited(Kind::Kimi, "hooks = [{ event = \"Stop\", command = \"x\" }]\n", true).is_err(), "an inline array is not changed");
        assert!(edited(Kind::Kimi, "", true).unwrap().contains("Kimi:Stop"));
        assert_eq!(edited(Kind::Kimi, "", false).unwrap(), "");
    }

    #[test]
    fn antigravity_s_hooks_keep_the_others() {
        let before = r#"{"mine": {"Stop": [{"type": "command", "command": "./done.sh"}]}}"#;
        let with = edited(Kind::Antigravity, before, true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&with).unwrap();
        assert_eq!(v["mine"]["Stop"][0]["command"], "./done.sh");
        assert!(v[crate::state::APP_ID]["Stop"][0]["command"].as_str().unwrap().ends_with("Antigravity:Stop"));
        assert!(has_hooks(Kind::Antigravity, &with));
        let without = edited(Kind::Antigravity, &with, false).unwrap();
        assert!(!has_hooks(Kind::Antigravity, &without) && without.contains("done.sh"));
        assert!(edited(Kind::Antigravity, "", true).is_ok());
        assert!(edited(Kind::Antigravity, "[1]", true).is_err());
    }

    #[test]
    fn only_a_terminal_of_these_takes_them() {
        let kimi = Profile { kind: Kind::Kimi, env: [("KIMI_CODE_HOME".to_string(), "/x/kimi".to_string())].into(), ..Default::default() };
        if cfg!(unix) {
            assert_eq!(config_file(&kimi), Some(PathBuf::from("/x/kimi/config.toml")));
        }
        assert_eq!(config_file(&Profile { acp: true, ..kimi.clone() }), None, "a chat tells what it does itself");
        assert_eq!(config_file(&Profile { kind: Kind::Claude, ..kimi }), None);
    }
}
