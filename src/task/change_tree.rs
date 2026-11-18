use super::*;

// ---------------------------------------------------------------- changes tree

#[derive(Debug, PartialEq)]
pub(super) enum ChangeNode {
    /// A folder; chains of folders holding one folder and no files show as
    /// one ("src/main/kotlin").
    Dir { path: String, label: String, depth: usize },
    File { ix: usize, depth: usize },
}

/// The changed files as the setting has them shown: a tree of folders, or
/// the files alone, each to be drawn with its folder after its name.
pub(super) fn changes_shown(paths: &[(usize, &str)]) -> Vec<ChangeNode> {
    if crate::state::with_settings(|s| s.changes_flat) {
        return paths.iter().map(|&(ix, _)| ChangeNode::File { ix, depth: 0 }).collect();
    }
    change_tree(paths)
}

/// The folder of a path, to follow a file's name where no tree shows it.
pub(super) fn folder_shown(path: &str) -> Option<String> {
    crate::state::with_settings(|s| s.changes_flat).then(|| path.rsplit_once('/').map(|(folder, _)| folder.to_string())).flatten()
}

/// Sorted paths as rows of a folder tree, folders first.
pub(super) fn change_tree(paths: &[(usize, &str)]) -> Vec<ChangeNode> {
    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        files: Vec<(String, usize)>,
    }
    let mut root = Node::default();
    for &(ix, path) in paths {
        let mut node = &mut root;
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap_or_default();
        for part in parts {
            node = node.dirs.entry(part.to_string()).or_default();
        }
        node.files.push((name.to_string(), ix));
    }
    fn walk(node: &Node, prefix: &str, depth: usize, out: &mut Vec<ChangeNode>) {
        for (name, child) in &node.dirs {
            let (mut label, mut path, mut child) = (name.clone(), format!("{prefix}{name}"), child);
            while child.files.is_empty() && child.dirs.len() == 1 {
                let (next, grandchild) = child.dirs.iter().next().unwrap();
                label = format!("{label}/{next}");
                path = format!("{path}/{next}");
                child = grandchild;
            }
            out.push(ChangeNode::Dir { path: path.clone(), label, depth });
            walk(child, &format!("{path}/"), depth + 1, out);
        }
        let mut files = node.files.clone();
        files.sort();
        for (_, ix) in files {
            out.push(ChangeNode::File { ix, depth });
        }
    }
    let mut out = Vec::new();
    walk(&root, "", 0, &mut out);
    out
}

#[cfg(test)]
mod change_tree_tests {
    use super::{ChangeNode, change_tree};

    #[test]
    fn a_list_has_each_file_once_and_its_folder_told() {
        let _one = crate::workspace::variants::tests::ONE_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let paths = [(0, "README.md"), (1, "src/task/mod.rs")];
        let before = crate::state::settings();
        crate::state::set_settings(crate::state::Settings { changes_flat: true, ..before.clone() });
        assert_eq!(super::changes_shown(&paths), vec![ChangeNode::File { ix: 0, depth: 0 }, ChangeNode::File { ix: 1, depth: 0 }]);
        assert_eq!((super::folder_shown("src/task/mod.rs").as_deref(), super::folder_shown("README.md")), (Some("src/task"), None));
        crate::state::set_settings(crate::state::Settings { changes_flat: false, ..before });
        assert!(matches!(super::changes_shown(&paths)[0], ChangeNode::Dir { .. }) && super::folder_shown("src/task/mod.rs").is_none());
    }

    /// A diff's files are in the order of the list beside them: as a tree a
    /// folder's folders come before its files, which is not git's order.
    #[test]
    fn a_diff_s_files_follow_the_tree_or_the_list() {
        let paths: Vec<String> = ["README.md", "src/sidebar.rs", "src/workspace/variants.rs", "src/task/mod.rs"].map(String::from).to_vec();
        let named = |order: Vec<usize>| order.into_iter().map(|ix| paths[ix].as_str()).collect::<Vec<_>>();
        assert_eq!(named(super::shown_order(&paths, false)), ["src/task/mod.rs", "src/workspace/variants.rs", "src/sidebar.rs", "README.md"]);
        assert_eq!(named(super::shown_order(&paths, true)), ["src/task/mod.rs", "README.md", "src/sidebar.rs", "src/workspace/variants.rs"], "a list is by name: mod, README, sidebar, variants");
        // Two of a name, by their folders.
        let same: Vec<String> = ["src/task/mod.rs", "src/acp/mod.rs", "src/agents/mod.rs"].map(String::from).to_vec();
        assert_eq!(super::shown_order(&same, true), [1, 2, 0]);
    }

    #[test]
    fn folders_compact_and_nest() {
        let paths = [(0, "README.md"), (1, "src/main/kotlin/App.kt"), (2, "src/main/kotlin/ui/View.kt"), (3, "src/test/T.kt")];
        let dir = |path: &str, label: &str, depth| ChangeNode::Dir { path: path.into(), label: label.into(), depth };
        assert_eq!(
            change_tree(&paths),
            vec![
                dir("src", "src", 0),
                dir("src/main/kotlin", "main/kotlin", 1),
                dir("src/main/kotlin/ui", "ui", 2),
                ChangeNode::File { ix: 2, depth: 3 },
                ChangeNode::File { ix: 1, depth: 2 },
                dir("src/test", "test", 1),
                ChangeNode::File { ix: 3, depth: 2 },
                ChangeNode::File { ix: 0, depth: 0 },
            ]
        );
    }
}

pub(super) fn location_of(range: &DiffLineRange) -> String {
    if range.start() == range.end() {
        format!("{}:{}", range.path(), range.start())
    } else {
        format!("{}:{}-{}", range.path(), range.start(), range.end())
    }
}
