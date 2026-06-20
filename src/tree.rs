//! `.gitignore` を尊重したファイルツリー（`ignore` クレートで走査）。
//!
//! 全体を一度走査してネスト構造を作り、展開状態に応じて可視行へ平坦化する。

use std::path::{Path, PathBuf};


pub struct TreeNode {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub expanded: bool,
    /// `.gitignore` 等で無視対象か（表示色を変える）。
    pub ignored: bool,
    pub children: Vec<TreeNode>,
}

/// 平坦化された 1 行ぶんの表示情報。
pub struct Row {
    pub depth: usize,
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub expanded: bool,
    pub ignored: bool,
}

pub struct Tree {
    root: TreeNode,
    pub selected: usize,
    rows: Vec<RowRef>,
}

/// 平坦化時に保持する、ノードへの経路（インデックス列）。
struct RowRef {
    path: PathBuf,
    is_dir: bool,
    indices: Vec<usize>,
}

impl Tree {
    pub fn new(root: &Path, is_ignored: &dyn Fn(&Path) -> bool) -> Self {
        let mut tree = Self {
            root: build_tree(root, is_ignored),
            selected: 0,
            rows: Vec::new(),
        };
        tree.rebuild_rows();
        tree
    }

    /// 表示用の行一覧を返す。
    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::with_capacity(self.rows.len());
        flatten(&self.root, 0, &mut out);
        out
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// 選択行がディレクトリなら展開状態をトグルする。ファイルなら何もしない。
    /// 戻り値は「ファイルが選択されていてそれを開くべきか」。
    pub fn activate(&mut self) -> Option<PathBuf> {
        let row = self.rows.get(self.selected)?;
        if row.is_dir {
            let indices = row.indices.clone();
            if let Some(node) = node_at_mut(&mut self.root, &indices) {
                node.expanded = !node.expanded;
            }
            self.rebuild_rows();
            None
        } else {
            Some(row.path.clone())
        }
    }

    /// 指定ファイルを可視化して選択する（祖先ディレクトリを展開）。
    /// ツリーに無い（gitignore 等）場合は何もしない。
    pub fn reveal(&mut self, path: &Path) {
        let Ok(rel) = path.strip_prefix(&self.root.path) else {
            return;
        };
        let comps: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        if comps.is_empty() {
            return;
        }

        // 祖先ディレクトリ（末尾=ファイル名 を除く）への indices を辿る。
        let mut indices: Vec<usize> = Vec::new();
        let mut node = &self.root;
        for name in &comps[..comps.len() - 1] {
            let Some(idx) = node.children.iter().position(|c| c.name == *name) else {
                return;
            };
            indices.push(idx);
            node = &node.children[idx];
        }

        // 各祖先を展開する。
        for k in 1..=indices.len() {
            if let Some(n) = node_at_mut(&mut self.root, &indices[..k]) {
                n.expanded = true;
            }
        }
        self.rebuild_rows();

        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.selected = i;
        }
    }

    /// 選択ディレクトリを畳む。ファイル上なら親へ移動する感覚で何もしない。
    pub fn collapse(&mut self) {
        if let Some(row) = self.rows.get(self.selected)
            && row.is_dir
        {
            let indices = row.indices.clone();
            if let Some(node) = node_at_mut(&mut self.root, &indices) {
                node.expanded = false;
            }
            self.rebuild_rows();
        }
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        collect_rows(&self.root, &mut Vec::new(), &mut rows);
        self.rows = rows;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }
}

/// 走査してネスト木を構築する。無視対象も含める（`ignored` を立てる）。
fn build_tree(root: &Path, is_ignored: &dyn Fn(&Path) -> bool) -> TreeNode {
    let mut root_node = TreeNode {
        name: display_name(root),
        path: root.to_path_buf(),
        is_dir: true,
        expanded: true,
        ignored: false,
        children: Vec::new(),
    };

    for entry in crate::finder::walk_visible(root, is_ignored) {
        let Ok(rel) = entry.abs.strip_prefix(root) else {
            continue;
        };
        insert(&mut root_node, rel, &entry.abs, entry.is_dir, entry.ignored);
    }

    sort_tree(&mut root_node);
    root_node
}

/// 相対パスの各コンポーネントを辿り、なければ作りながら葉を挿入する。
fn insert(node: &mut TreeNode, rel: &Path, full: &Path, is_dir: bool, ignored: bool) {
    let mut current = node;
    let components: Vec<_> = rel.components().collect();
    for (i, comp) in components.iter().enumerate() {
        let name = comp.as_os_str().to_string_lossy().to_string();
        let is_leaf = i + 1 == components.len();
        let pos = current.children.iter().position(|c| c.name == name);
        let idx = match pos {
            Some(idx) => idx,
            None => {
                current.children.push(TreeNode {
                    name,
                    path: full.to_path_buf(),
                    is_dir: if is_leaf { is_dir } else { true },
                    expanded: false,
                    // 葉のみ実際の無視フラグ。中間ディレクトリは非無視。
                    ignored: is_leaf && ignored,
                    children: Vec::new(),
                });
                current.children.len() - 1
            }
        };
        current = &mut current.children[idx];
    }
}

fn sort_tree(node: &mut TreeNode) {
    node.children.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    for child in &mut node.children {
        sort_tree(child);
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// 可視行（展開状態を考慮）を、経路インデックス付きで収集する。
fn collect_rows(node: &TreeNode, path: &mut Vec<usize>, out: &mut Vec<RowRef>) {
    for (i, child) in node.children.iter().enumerate() {
        path.push(i);
        out.push(RowRef {
            path: child.path.clone(),
            is_dir: child.is_dir,
            indices: path.clone(),
        });
        if child.is_dir && child.expanded {
            collect_rows(child, path, out);
        }
        path.pop();
    }
}

/// 表示用の `Row` を収集する（描画専用、軽量）。
fn flatten(node: &TreeNode, depth: usize, out: &mut Vec<Row>) {
    for child in &node.children {
        out.push(Row {
            depth,
            name: child.name.clone(),
            path: child.path.clone(),
            is_dir: child.is_dir,
            expanded: child.expanded,
            ignored: child.ignored,
        });
        if child.is_dir && child.expanded {
            flatten(child, depth + 1, out);
        }
    }
}

fn node_at_mut<'a>(root: &'a mut TreeNode, indices: &[usize]) -> Option<&'a mut TreeNode> {
    let mut current = root;
    for &idx in indices {
        current = current.children.get_mut(idx)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_expands_and_selects_nested_file() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut tree = Tree::new(&root, &|_| false);
        let target = root.join("src").join("app.rs");
        tree.reveal(&target);
        let rows = tree.rows();
        let sel = &rows[tree.selected];
        assert_eq!(sel.path, target, "selected row should be the revealed file");
        assert!(!sel.is_dir);
    }

    #[test]
    fn tree_shows_dotfiles() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let tree = Tree::new(&root, &|_| false);
        // 直下の .gitignore（ドットファイル）がツリーに出る。
        let names: Vec<String> = tree.rows().iter().map(|r| r.name.clone()).collect();
        assert!(
            names.iter().any(|n| n.contains(".gitignore")),
            "dotfile not shown: {names:?}"
        );
    }
}
