//! Shortcuts are written the Linux and Windows way ("ctrl-shift-n"); on
//! macOS they become what the platform and JetBrains' macOS keymap expect:
//! Cmd for Ctrl, and a few of their own where Cmd would clash.

/// The keystrokes to bind for one written the Linux way.
pub fn key(linux: &str) -> String {
    if !cfg!(target_os = "macos") {
        return linux.to_string();
    }
    // Keys pressed one after another are each put the platform's way.
    if linux.contains(' ') {
        return linux.split(' ').map(key).collect::<Vec<_>>().join(" ");
    }
    match linux {
        // Cmd+` switches windows on macOS.
        "ctrl-`" => "ctrl-`",
        // Cmd+Space is Spotlight's; JetBrains completes on Ctrl+Space there too.
        "ctrl-space" => "ctrl-space",
        "ctrl-pagedown" => "cmd-shift-]",
        "ctrl-pageup" => "cmd-shift-[",
        // Opt+arrows move by word in an editor there.
        "alt-left" => "cmd-[",
        "alt-right" => "cmd-]",
        "ctrl-w" => "alt-up",
        "ctrl-shift-w" => "alt-down",
        "ctrl-shift-n" => "cmd-shift-o",
        other => return other.strip_prefix("ctrl-").map_or(other.to_string(), |rest| format!("cmd-{rest}")),
    }
    .to_string()
}

/// "Ctrl+Shift+T" as this platform shows it: "⌘⇧T" on macOS. Chords
/// separated by spaces ("Ctrl+E Ctrl+E") each.
pub fn label(linux: &str) -> String {
    linux.split(' ').map(chord).collect::<Vec<_>>().join(" ")
}

fn chord(linux: &str) -> String {
    if !cfg!(target_os = "macos") {
        return linux.to_string();
    }
    let binding = linux
        .split('+')
        .map(|part| match part {
            "PgDn" => "pagedown".to_string(),
            "PgUp" => "pageup".to_string(),
            "Left" | "Right" | "Up" | "Down" => part.to_lowercase(),
            other => other.to_lowercase(),
        })
        .collect::<Vec<_>>()
        .join("-");
    let mac = key(&binding);
    let mut out = String::new();
    let parts: Vec<&str> = mac.split('-').collect();
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        out.push_str(&match (*part, last) {
            ("cmd", false) => "⌘".to_string(),
            ("ctrl", false) => "⌃".to_string(),
            ("alt", false) => "⌥".to_string(),
            ("shift", false) => "⇧".to_string(),
            ("up", true) => "↑".to_string(),
            ("down", true) => "↓".to_string(),
            ("left", true) => "←".to_string(),
            ("right", true) => "→".to_string(),
            ("enter", true) => "↩".to_string(),
            (k, _) if k.len() == 1 => k.to_uppercase(),
            (k, _) => {
                let mut c = k.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{key, label};

    #[test]
    fn every_key_of_a_sequence_is_the_macs() {
        if !cfg!(target_os = "macos") {
            return;
        }
        assert_eq!(key("ctrl-k ctrl-t"), "cmd-k cmd-t", "not the first alone");
        assert_eq!(key("ctrl-shift-n"), "cmd-shift-o");
    }

    #[test]
    fn linux_keys_stay() {
        if cfg!(target_os = "macos") {
            return;
        }
        assert_eq!(key("ctrl-shift-n"), "ctrl-shift-n");
        assert_eq!(label("Ctrl+E Ctrl+E"), "Ctrl+E Ctrl+E");
    }
}
