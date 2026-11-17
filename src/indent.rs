//! How a file is indented: what `.editorconfig` says, else what the file
//! itself does, else what its language does. What Tab inserts and what the
//! language server is told when it formats.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Indent {
    /// Columns a level takes, and a tab shows as.
    pub size: usize,
    pub tabs: bool,
}

impl Default for Indent {
    fn default() -> Self {
        Self { size: 4, tabs: false }
    }
}

/// Lines looked at to tell a file's indent.
const SAMPLE: usize = 400;

/// The indent of the file at `path` with `text`; `.editorconfig` files are
/// looked for from its folder up to `root`.
pub fn of(path: &Path, text: &str, root: &Path) -> Indent {
    let by_language = by_language(path);
    let by_file = detect(text);
    let mut indent = by_file.unwrap_or(by_language);
    let (style, size) = editorconfig(path, root);
    if let Some(tabs) = style {
        indent.tabs = tabs;
    }
    if let Some(size) = size {
        indent.size = size;
    } else if style.is_some() && by_file.is_none() {
        indent.size = by_language.size;
    }
    indent
}

fn by_language(path: &Path) -> Indent {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if name == "makefile" || name == "gnumakefile" || ext == "go" || ext == "mk" {
        return Indent { size: 4, tabs: true };
    }
    let two = ["js", "jsx", "ts", "tsx", "mjs", "cjs", "json", "yml", "yaml", "html", "css", "scss", "vue", "rb", "lua", "dart", "nix", "tf"];
    Indent { size: if two.contains(&ext.as_str()) { 2 } else { 4 }, tabs: false }
}

/// Tabs when more indented lines start with a tab; else the step the
/// indents of space-indented lines most often differ by.
pub fn detect(text: &str) -> Option<Indent> {
    let (mut tabbed, mut spaced) = (0usize, 0usize);
    let mut steps = [0usize; 9];
    let mut previous = 0usize;
    for line in text.lines().take(SAMPLE) {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tabbed += 1;
            continue;
        }
        let spaces = line.len() - line.trim_start_matches(' ').len();
        // A block comment's " * " continues no indent.
        if spaces > 0 && !line.trim_start().starts_with('*') {
            spaced += 1;
            let step = spaces.abs_diff(previous);
            if (2..=8).contains(&step) {
                steps[step] += 1;
            }
        }
        if !line.trim_start().starts_with('*') {
            previous = spaces;
        }
    }
    if tabbed == 0 && spaced == 0 {
        return None;
    }
    if tabbed > spaced {
        return Some(Indent { size: 4, tabs: true });
    }
    let size = (2..=8).max_by_key(|&s| (steps[s], s == 4, s == 2)).filter(|&s| steps[s] > 0)?;
    Some(Indent { size, tabs: false })
}

/// `indent_style` (tabs or not) and `indent_size` for `path`, from the
/// `.editorconfig` files between it and `root`, the nearest winning.
fn editorconfig(path: &Path, root: &Path) -> (Option<bool>, Option<usize>) {
    let mut configs = Vec::new();
    let mut dir = path.parent();
    while let Some(at) = dir {
        if let Ok(text) = std::fs::read_to_string(at.join(".editorconfig")) {
            let top = text.lines().any(|l| l.split_once('=').is_some_and(|(k, v)| k.trim() == "root" && v.trim().eq_ignore_ascii_case("true")));
            configs.push((at.to_path_buf(), text));
            if top {
                break;
            }
        }
        if at == root || !at.starts_with(root) {
            break;
        }
        dir = at.parent();
    }
    let (mut style, mut size, mut tab_width) = (None, None, None);
    // Farthest first: the nearer file's sections are read after and win.
    for (dir, text) in configs.iter().rev() {
        let relative = path.strip_prefix(dir).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        let mut applies = false;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with(['#', ';'])) {
            if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                applies = section_matches(section, &relative);
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            if !applies {
                continue;
            }
            let value = value.trim().to_lowercase();
            match key.trim().to_lowercase().as_str() {
                "indent_style" => style = Some(value == "tab"),
                "indent_size" if value == "tab" => size = None,
                "indent_size" => size = value.parse().ok().or(size),
                "tab_width" => tab_width = value.parse().ok().or(tab_width),
                _ => {}
            }
        }
    }
    (style, size.or(tab_width))
}

/// The globs editorconfig sections use most: `*`, `*.ext`, `*.{a,b}`, a name,
/// and those with a folder (`src/*.rs`, `**.go`).
fn section_matches(section: &str, relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    let (pattern, subject) = if section.contains('/') { (section.trim_start_matches('/'), relative) } else { (section, name) };
    let alternatives: Vec<String> = match (pattern.find('{'), pattern.find('}')) {
        (Some(open), Some(close)) if open < close => {
            pattern[open + 1..close].split(',').map(|alt| format!("{}{}{}", &pattern[..open], alt, &pattern[close + 1..])).collect()
        }
        _ => vec![pattern.to_string()],
    };
    alternatives.iter().any(|alt| glob(&alt.replace("**", "*"), subject))
}

/// `*` for any run of characters (a folder's `/` too: editorconfig's `**`
/// already became `*`), `?` for one.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_tells_its_indent() {
        assert_eq!(detect("fn a() {\n    b();\n    if c {\n        d();\n    }\n}\n"), Some(Indent { size: 4, tabs: false }));
        assert_eq!(detect("a:\n  b: 1\n  c:\n    d: 2\n"), Some(Indent { size: 2, tabs: false }));
        assert_eq!(detect("func a() {\n\tb()\n\tif c {\n\t\td()\n\t}\n}\n"), Some(Indent { size: 4, tabs: true }));
        assert_eq!(detect("/**\n * doc\n */\nfn a() {}\n"), None, "a comment's star is no indent");
        assert_eq!(detect("one\ntwo\n"), None);
    }

    #[test]
    fn editorconfig_wins_the_nearest_last() {
        let root = std::env::temp_dir().join(format!("ideae-indent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(root.join(".editorconfig"), "root = true\n[*]\nindent_style = space\nindent_size = 4\n[*.{go,mk}]\nindent_style = tab\n[Makefile]\nindent_style = tab\n").unwrap();
        std::fs::write(root.join("web/.editorconfig"), "[*.ts]\nindent_size = 2\n").unwrap();
        let at = |p: &str, text: &str| of(&root.join(p), text, &root);
        assert_eq!(at("main.go", ""), Indent { size: 4, tabs: true });
        assert_eq!(at("Makefile", "all:\n    x\n"), Indent { size: 4, tabs: true }, "the config over the file");
        assert_eq!(at("web/app.ts", "a\n    b\n"), Indent { size: 2, tabs: false }, "the nearer one");
        assert_eq!(at("lib.rs", "a\n  b\n"), Indent { size: 4, tabs: false });
        let _ = std::fs::remove_dir_all(&root);
        // No config: the file, then its language.
        let nowhere = std::env::temp_dir().join("ideae-indent-none");
        assert_eq!(of(&nowhere.join("x.go"), "", &nowhere), Indent { size: 4, tabs: true });
        assert_eq!(of(&nowhere.join("x.ts"), "", &nowhere), Indent { size: 2, tabs: false });
        assert_eq!(of(&nowhere.join("x.py"), "a:\n  b\n", &nowhere), Indent { size: 2, tabs: false });
    }
}
