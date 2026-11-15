//! The app in the desktop: a launcher entry pinned to the GNOME dash, kept
//! pointing at the binary when it moves, notifications, and restarting. On
//! Windows there is no dash to pin to.

use std::path::PathBuf;

use crate::state::APP_ID;

fn desktop_id() -> String {
    format!("{APP_ID}.desktop")
}

const ICON: &str = include_str!("../assets/icon/ideae.svg");

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

fn desktop_file() -> Option<PathBuf> {
    Some(data_home()?.join("applications").join(desktop_id()))
}

/// The running binary; after it was replaced on disk Linux reports the old
/// path with " (deleted)", and the new file is there under the plain name.
pub fn exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let s = exe.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(plain) => PathBuf::from(plain),
        None => exe,
    })
}

fn entry(exe: &std::path::Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={name}\nComment=IDE for the agentic age\nExec=\"{}\" %F\nIcon={APP_ID}\nTerminal=false\nCategories=Development;\nStartupWMClass={APP_ID}\n",
        exe.display(),
        name = if cfg!(debug_assertions) { "ideae (dev)" } else { "ideae" },
    )
}

fn favorites() -> Option<Vec<String>> {
    let out = crate::procs::command("gsettings").args(["get", "org.gnome.shell", "favorite-apps"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let list = text.trim().trim_start_matches("@as").trim().trim_start_matches('[').trim_end_matches(']');
    Some(list.split(',').map(|s| s.trim().trim_matches('\'').to_string()).filter(|s| !s.is_empty()).collect())
}

fn set_favorites(list: &[String]) {
    let value = format!("[{}]", list.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", "));
    let _ = crate::procs::command("gsettings").args(["set", "org.gnome.shell", "favorite-apps", &value]).status();
}

/// A desktop notification. On Windows a balloon from PowerShell, which needs
/// no app registration as a toast does.
#[cfg_attr(all(unix, not(target_os = "macos")), allow(dead_code))]
pub fn notify(title: &str, body: &str) {
    #[cfg(windows)]
    let child = {
        let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; $n = New-Object System.Windows.Forms.NotifyIcon; \
             $n.Icon = [System.Drawing.Icon]::ExtractAssociatedIcon({exe}); $n.Visible = $true; \
             $n.ShowBalloonTip(5000, {title}, {body}, 'None'); Start-Sleep 6; $n.Dispose()",
            exe = quote(&exe().map(|e| e.display().to_string()).unwrap_or_default()),
            title = quote(title),
            body = quote(body),
        );
        crate::procs::command("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", &script]).spawn()
    };
    #[cfg(target_os = "macos")]
    let child = osascript_notification(title, body).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let child = crate::procs::command("notify-send").args(["-a", APP_ID, title, body]).spawn();
    // Waited for off the UI thread, or each one stays a zombie.
    if let Ok(mut child) = child {
        std::thread::spawn(move || child.wait());
    }
}

/// A notification that runs `on_click` when it is clicked. On Linux that is
/// notify-send's default action, waited for on a thread of its own; a
/// Windows balloon reports no clicks, so there it is a plain one.
pub fn notify_with(title: &str, body: &str, on_click: impl FnOnce() + Send + 'static) {
    // Neither a Windows balloon nor an AppleScript notification reports clicks.
    #[cfg(any(windows, target_os = "macos"))]
    {
        drop(on_click);
        notify(title, body);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let child = crate::procs::command("notify-send")
            .args(["-a", APP_ID, "-A", "default=Open", title, body])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(child) = child {
            std::thread::spawn(move || {
                if let Ok(out) = child.wait_with_output() {
                    if String::from_utf8_lossy(&out.stdout).trim() == "default" {
                        on_click();
                    }
                }
            });
        }
    }
}

/// Starts a window on the task at `path` (from a notification the daemon
/// raised while no window was open).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn open_window_on(path: &std::path::Path) {
    if let Ok(exe) = exe() {
        if let Ok(mut child) = crate::procs::command(exe).arg("--task").arg(path).spawn() {
            std::thread::spawn(move || child.wait());
        }
    }
}

/// `display notification` from AppleScript: the strings go in as
/// arguments, so nothing in them is read as script.
#[cfg(target_os = "macos")]
fn osascript_notification(title: &str, body: &str) -> std::process::Command {
    let mut command = crate::procs::command("osascript");
    command
        .args(["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run"])
        .args([title, body]);
    command
}

pub fn can_pin() -> bool {
    cfg!(target_os = "linux")
}

pub fn is_pinned() -> bool {
    if !can_pin() {
        return false;
    }
    favorites().is_some_and(|f| f.iter().any(|s| *s == desktop_id()))
}

/// Writes the launcher entry and icon, and adds it to the dash.
pub fn pin() -> Result<(), String> {
    write_entry()?;
    let mut list = favorites().ok_or("the GNOME dash is not available")?;
    if !list.iter().any(|s| *s == desktop_id()) {
        list.push(desktop_id().into());
        set_favorites(&list);
    }
    Ok(())
}

pub fn unpin() {
    if let Some(mut list) = favorites() {
        list.retain(|s| *s != desktop_id());
        set_favorites(&list);
    }
}

fn icon_file() -> Option<PathBuf> {
    Some(data_home()?.join(format!("icons/hicolor/scalable/apps/{APP_ID}.svg")))
}

fn write_entry() -> Result<(), String> {
    let exe = exe().map_err(|e| e.to_string())?;
    let file = desktop_file().ok_or("no home directory")?;
    crate::state::write_atomic(&file, entry(&exe).as_bytes()).map_err(|e| e.to_string())?;
    if let Some(icon) = icon_file() {
        let _ = crate::state::write_atomic(&icon, ICON.as_bytes());
    }
    Ok(())
}

/// An entry written earlier points at where the binary was then, and its
/// icon is the one of that build: follow both.
pub fn refresh_entry() {
    if !can_pin() {
        return;
    }
    let (Some(file), Ok(exe)) = (desktop_file(), exe()) else { return };
    if let Ok(current) = std::fs::read_to_string(&file) {
        let stale_icon = icon_file().is_some_and(|icon| std::fs::read_to_string(icon).is_ok_and(|old| old != ICON));
        if (current != entry(&exe) || stale_icon) && current.contains(&format!("StartupWMClass={APP_ID}\n")) {
            let _ = write_entry();
        }
    }
}

/// Starts a new instance with the same arguments; the caller quits.
pub fn spawn_again() -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    if let Some(app) = linked_bundle() {
        let mut open = crate::procs::command("open");
        open.arg("-n");
        // Launch Services starts the app with its own environment, not ours.
        for (name, value) in std::env::vars().filter(|(name, _)| name.starts_with("IDEAE_")) {
            open.arg("--env").arg(format!("{name}={value}"));
        }
        return open.arg(app).arg("--args").args(std::env::args_os().skip(1)).spawn().map(|_| ());
    }
    crate::procs::command(exe()?).args(std::env::args_os().skip(1)).spawn().map(|_| ())
}

/// The .app this process was opened from when its binary is a symlink out
/// of the bundle (scripts/macos-app.sh --link): started directly, the
/// binary would be a second, iconless app in the Dock.
#[cfg(target_os = "macos")]
fn linked_bundle() -> Option<PathBuf> {
    let out = crate::procs::command("lsappinfo")
        .args(["info", "-only", "bundlepath", &std::process::id().to_string()])
        .output()
        .ok()?;
    let app = bundle_path(&String::from_utf8_lossy(&out.stdout))?;
    let inside = exe().is_ok_and(|exe| exe.starts_with(&app));
    (!inside && app.join("Contents/Info.plist").is_file()).then_some(app)
}

#[cfg(any(target_os = "macos", test))]
fn bundle_path(lsappinfo: &str) -> Option<PathBuf> {
    let path = lsappinfo.split_once("bundle path=\"")?.1.split_once("\"\n")?.0;
    path.ends_with(".app").then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_path_from_lsappinfo() {
        let out = "[ NULL ]  ASN:0x0-0x1: \n    bundleID=[ NULL ] \n    bundle path=\"/Applications/my \"x\" ideae.app\"\n    executable path=[ NULL ] \n";
        assert_eq!(bundle_path(out), Some(PathBuf::from("/Applications/my \"x\" ideae.app")));
        assert_eq!(bundle_path("    bundle path=[ NULL ] \n"), None);
        assert_eq!(bundle_path("    bundle path=\"/usr/bin/thing\"\n"), None);
    }
}
