use super::*;

// ---------------------------------------------------------------- changes tree

#[derive(Debug, PartialEq)]
pub(super) enum ChangeNode {
    /// A folder; chains of folders holding one folder and no files show as
    /// one ("src/main/kotlin").
    Dir { path: String, label: String, depth: usize },
    File { ix: usize, depth: usize },
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
