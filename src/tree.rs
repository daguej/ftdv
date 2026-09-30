use crate::parser::FileDiff;
use std::collections::HashSet;

#[derive(Clone)]
pub struct FileTreeItem {
    pub name: String,
    pub full_path: String,
    pub is_directory: bool,
    pub depth: usize,
    pub file_diff: Option<FileDiff>,
    pub is_last_child: bool,
    pub parent_is_last: Vec<bool>, // Track if each ancestor is the last child
    pub is_expanded: bool,         // Track if directory is expanded
    // Directory statistics (only used when directory is collapsed)
    pub dir_file_count: usize,  // Total files in this directory (recursive)
    pub dir_added_lines: usize, // Total added lines in this directory (recursive)
    pub dir_removed_lines: usize, // Total removed lines in this directory (recursive)
}

#[derive(Clone)]
struct TreeNode {
    name: String,
    full_path: String,
    is_directory: bool,
    file_diff: Option<FileDiff>,
    children: Vec<TreeNode>,
    // Cached statistics for directory
    file_count: usize,
    added_lines: usize,
    removed_lines: usize,
}

pub struct FileTreeBuilder;

impl FileTreeBuilder {
    pub fn build_file_tree(file_diffs: &[FileDiff]) -> Vec<FileTreeItem> {
        Self::build_file_tree_with_collapsed(file_diffs, &HashSet::new())
    }

    pub fn build_file_tree_with_collapsed(
        file_diffs: &[FileDiff],
        collapsed_dirs: &HashSet<String>,
    ) -> Vec<FileTreeItem> {
        // First, build a true tree structure like diffnav does
        let root = Self::build_tree_structure(file_diffs);

        // Then flatten it into display order while preserving hierarchy
        let mut result = Vec::new();
        Self::flatten_tree_with_collapsed(&root, &mut Vec::new(), &mut result, collapsed_dirs);

        result
    }

    fn build_tree_structure(file_diffs: &[FileDiff]) -> TreeNode {
        let mut root = TreeNode {
            name: "".to_string(),
            full_path: "".to_string(),
            is_directory: true,
            file_diff: None,
            children: Vec::new(),
            file_count: 0,
            added_lines: 0,
            removed_lines: 0,
        };

        // Sort files first (like diffnav does)
        let mut sorted_diffs = file_diffs.to_vec();
        sorted_diffs.sort_by(|a, b| {
            let dir_a = std::path::Path::new(&a.filename)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string());
            let dir_b = std::path::Path::new(&b.filename)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string());

            let name_a = std::path::Path::new(&a.filename)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| a.filename.clone());
            let name_b = std::path::Path::new(&b.filename)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| b.filename.clone());

            // Apply diffnav sorting logic
            if dir_a != "." && dir_b != "." && dir_a == dir_b {
                return name_a.to_lowercase().cmp(&name_b.to_lowercase());
            }

            if dir_a != "." && dir_b == "." {
                return std::cmp::Ordering::Less;
            }
            if dir_b != "." && dir_a == "." {
                return std::cmp::Ordering::Greater;
            }

            if dir_a != "." && dir_b != "." {
                if dir_a.starts_with(&format!("{dir_b}/")) {
                    return std::cmp::Ordering::Less;
                }
                if dir_b.starts_with(&format!("{dir_a}/")) {
                    return std::cmp::Ordering::Greater;
                }
            }

            a.filename.cmp(&b.filename)
        });

        // Build tree incrementally like diffnav
        for file_diff in sorted_diffs {
            let filename = file_diff.filename.clone();
            Self::add_file_to_tree(&mut root, &filename, Some(file_diff));
        }

        // Sort all children recursively
        Self::sort_tree_children(&mut root);

        // Calculate directory statistics
        Self::calculate_directory_stats(&mut root);

        root
    }

    fn add_file_to_tree(root: &mut TreeNode, path: &str, file_diff: Option<FileDiff>) {
        let parts: Vec<&str> = path.split('/').collect();
        let mut current = root;

        // Navigate/create directory structure
        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                // This is the file itself
                let (added, removed) = if let Some(ref fd) = file_diff {
                    (fd.added_lines, fd.removed_lines)
                } else {
                    (0, 0)
                };

                current.children.push(TreeNode {
                    name: part.to_string(),
                    full_path: path.to_string(),
                    is_directory: false,
                    file_diff: file_diff.clone(),
                    children: Vec::new(),
                    file_count: 1,
                    added_lines: added,
                    removed_lines: removed,
                });
            } else {
                // This is a directory component
                let dir_path = parts[..=i].join("/");

                // Find or create directory
                let child_index = current
                    .children
                    .iter()
                    .position(|c| c.name == *part && c.is_directory);
                if let Some(index) = child_index {
                    current = &mut current.children[index];
                } else {
                    current.children.push(TreeNode {
                        name: part.to_string(),
                        full_path: dir_path,
                        is_directory: true,
                        file_diff: None,
                        children: Vec::new(),
                        file_count: 0,
                        added_lines: 0,
                        removed_lines: 0,
                    });
                    let last_index = current.children.len() - 1;
                    current = &mut current.children[last_index];
                }
            }
        }
    }

    fn sort_tree_children(node: &mut TreeNode) {
        node.children
            .sort_by(|a, b| match (a.is_directory, b.is_directory) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            });

        // Recursively sort children
        for child in &mut node.children {
            Self::sort_tree_children(child);
        }
    }

    fn calculate_directory_stats(node: &mut TreeNode) -> (usize, usize, usize) {
        if !node.is_directory {
            // For files, return their own stats
            return (node.file_count, node.added_lines, node.removed_lines);
        }

        let mut total_files = 0;
        let mut total_added = 0;
        let mut total_removed = 0;

        // Recursively calculate stats for all children
        for child in &mut node.children {
            let (files, added, removed) = Self::calculate_directory_stats(child);
            total_files += files;
            total_added += added;
            total_removed += removed;
        }

        // Update this directory's stats
        node.file_count = total_files;
        node.added_lines = total_added;
        node.removed_lines = total_removed;

        (total_files, total_added, total_removed)
    }

    /// Append a node's children to `result` in display order, descending into expanded
    /// directories. `ancestors_last` holds, for each ancestor level, whether no connector
    /// line continues below that ancestor.
    fn flatten_tree_with_collapsed(
        node: &TreeNode,
        ancestors_last: &mut Vec<bool>,
        result: &mut Vec<FileTreeItem>,
        collapsed_dirs: &HashSet<String>,
    ) {
        let depth = ancestors_last.len();
        for (i, child) in node.children.iter().enumerate() {
            let is_last_child = i == node.children.len() - 1;
            let is_expanded = !collapsed_dirs.contains(&child.full_path);

            result.push(FileTreeItem {
                name: child.name.clone(),
                full_path: child.full_path.clone(),
                is_directory: child.is_directory,
                depth,
                file_diff: child.file_diff.clone(),
                is_last_child,
                parent_is_last: ancestors_last.clone(),
                is_expanded,
                dir_file_count: child.file_count,
                dir_added_lines: child.added_lines,
                dir_removed_lines: child.removed_lines,
            });

            if child.is_directory && is_expanded {
                // Top-level items have no connector, so no line continues below them
                ancestors_last.push(depth == 0 || is_last_child);
                Self::flatten_tree_with_collapsed(child, ancestors_last, result, collapsed_dirs);
                ancestors_last.pop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_diff(filename: &str) -> FileDiff {
        FileDiff {
            filename: filename.to_string(),
            old_path: None,
            new_path: None,
            content: String::new(),
            added_lines: 0,
            removed_lines: 0,
            diff_key: None,
            change: None,
        }
    }

    /// Tree lines as render_file_list draws them
    fn tree_prefixes(items: &[FileTreeItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| {
                let mut prefix: String = item
                    .parent_is_last
                    .iter()
                    .map(|&last| if last { "  " } else { "│ " })
                    .collect();
                if item.depth > 0 {
                    prefix.push_str(if item.is_last_child { "╰ " } else { "├ " });
                }
                prefix + &item.name
            })
            .collect()
    }

    #[test]
    fn test_tree_connectors() {
        let diffs = ["d/a", "d/e/x", "d/e/y", "d/z", "top"].map(file_diff);
        let items = FileTreeBuilder::build_file_tree(&diffs);

        assert_eq!(
            tree_prefixes(&items),
            ["d", "  ├ e", "  │ ├ x", "  │ ╰ y", "  ├ a", "  ╰ z", "top",]
        );
    }

    #[test]
    fn test_tree_connectors_with_collapsed_directory() {
        let diffs = ["d/a", "d/e/x", "d/z"].map(file_diff);
        let collapsed = HashSet::from(["d/e".to_string()]);
        let items = FileTreeBuilder::build_file_tree_with_collapsed(&diffs, &collapsed);

        assert_eq!(tree_prefixes(&items), ["d", "  ├ e", "  ├ a", "  ╰ z"]);
        assert!(!items[1].is_expanded);
    }
}
