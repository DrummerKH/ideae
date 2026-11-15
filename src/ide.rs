//! External editors installed on this machine, to open a file at a line.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
};

#[derive(Clone)]
pub struct Ide {
    pub name: &'static str,
    program: PathBuf,
    style: Style,
}

#[derive(Clone, Copy)]
enum Style {
    /// JetBrains: `--line N file`.
    JetBrains,
    /// Zed, Sublime: `file:line`.
    Colon,
    /// VS Code: `-g file:line`.
    Goto,
}

const KNOWN: &[(&str, &str, Style)] = &[
    ("GoLand", "goland", Style::JetBrains),
    ("IntelliJ IDEA", "idea", Style::JetBrains),
    ("PhpStorm", "phpstorm", Style::JetBrains),
    ("WebStorm", "webstorm", Style::JetBrains),
    ("PyCharm", "pycharm", Style::JetBrains),
    ("RustRover", "rustrover", Style::JetBrains),
    ("CLion", "clion", Style::JetBrains),
    ("Android Studio", "studio", Style::JetBrains),
    ("DataGrip", "datagrip", Style::JetBrains),
    ("Fleet", "fleet", Style::Colon),
    ("Zed", "zed", Style::Colon),
    ("VS Code", "code", Style::Goto),
    ("Sublime Text", "subl", Style::Colon),
];

/// Found once: PATH, snap, and JetBrains Toolbox's launcher scripts.
pub fn installed() -> &'static [Ide] {
    static FOUND: OnceLock<Vec<Ide>> = OnceLock::new();
    FOUND.get_or_init(|| {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        dirs.push(PathBuf::from("/snap/bin"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(Path::new(&home).join(".local/share/JetBrains/Toolbox/scripts"));
            dirs.push(Path::new(&home).join("Library/Application Support/JetBrains/Toolbox/scripts"));
        }
        KNOWN
            .iter()
            .filter_map(|&(name, bin, style)| {
                let program = dirs.iter().map(|d| d.join(bin)).find(|p| p.is_file())?;
                Some(Ide { name, program, style })
            })
            .collect()
    })
}

impl Ide {
    /// Opens `file` at `line` (1-based) in this editor, detached.
    pub fn open(&self, file: &Path, line: u32) {
        let mut command = crate::procs::command(&self.program);
        match self.style {
            Style::JetBrains => command.arg("--line").arg(line.to_string()).arg(file),
            Style::Colon => command.arg(format!("{}:{line}", file.display())),
            Style::Goto => command.arg("-g").arg(format!("{}:{line}", file.display())),
        };
        let _ = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
}
