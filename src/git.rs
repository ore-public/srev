//! git2 を用いた作業ツリー vs HEAD の状態取得と差分。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use git2::{Diff, DiffFormat, DiffOptions, Repository, Status};

/// ファイルの変更種別（ツリー表示と差分の見出しに使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

impl FileStatus {
    pub fn letter(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Untracked => '?',
        }
    }
}

/// 差分 1 行の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Hunk,
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffKind,
    /// 旧ファイル側の行番号（1 始まり）。削除・文脈行のみ。
    pub old_lineno: Option<u32>,
    /// 新ファイル側の行番号（1 始まり）。追加・文脈行のみ。
    pub new_lineno: Option<u32>,
    pub content: String,
}

pub struct GitInfo {
    repo: Repository,
    workdir: PathBuf,
}

impl GitInfo {
    /// `root` から git リポジトリを探す。見つからなければ `None`。
    pub fn discover(root: &Path) -> Option<Self> {
        let repo = Repository::discover(root).ok()?;
        let workdir = repo.workdir()?.to_path_buf();
        Some(Self { repo, workdir })
    }

    /// 変更ファイルの一覧（絶対パス → 状態）。
    pub fn statuses(&self) -> HashMap<PathBuf, FileStatus> {
        let mut map = HashMap::new();
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        let Ok(statuses) = self.repo.statuses(Some(&mut opts)) else {
            return map;
        };
        for entry in statuses.iter() {
            let Ok(rel) = entry.path() else { continue };
            let s = entry.status();
            let kind = if s.intersects(Status::WT_NEW) {
                FileStatus::Untracked
            } else if s.intersects(Status::INDEX_NEW) {
                FileStatus::Added
            } else if s.intersects(Status::WT_DELETED | Status::INDEX_DELETED) {
                FileStatus::Deleted
            } else if s.intersects(Status::WT_RENAMED | Status::INDEX_RENAMED) {
                FileStatus::Renamed
            } else if s.intersects(Status::WT_MODIFIED | Status::INDEX_MODIFIED) {
                FileStatus::Modified
            } else {
                continue;
            };
            map.insert(self.workdir.join(rel), kind);
        }
        map
    }

    /// パスが `.gitignore` 等で無視対象か（作業ディレクトリ相対で判定）。
    pub fn is_ignored(&self, abs: &Path) -> bool {
        let rel = abs.strip_prefix(&self.workdir).unwrap_or(abs);
        self.repo.is_path_ignored(rel).unwrap_or(false)
    }

    /// 指定ファイルの作業ツリー vs HEAD 差分を行リストとして返す。
    pub fn diff_file(&self, path: &Path) -> Option<Vec<DiffLine>> {
        let rel = path.strip_prefix(&self.workdir).ok()?;

        let head_tree = self
            .repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .and_then(|c| c.tree().ok());

        let mut opts = DiffOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .show_untracked_content(true)
            .pathspec(rel.to_string_lossy().to_string());

        let diff = self
            .repo
            .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))
            .ok()?;

        Some(collect_diff_lines(&diff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn detects_modified_file_and_diff() {
        let dir = std::env::temp_dir().join(format!("srev_git_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "tester"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        // 変更を入れる
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();

        let info = GitInfo::discover(&dir).expect("repo");
        let statuses = info.statuses();
        let (path, status) = statuses
            .iter()
            .find(|(p, _)| p.file_name().unwrap() == "a.txt")
            .expect("a.txt in statuses");
        assert_eq!(*status, FileStatus::Modified);

        let diff = info.diff_file(path).expect("diff");
        let added: Vec<_> = diff
            .iter()
            .filter(|l| l.kind == DiffKind::Add)
            .map(|l| l.content.as_str())
            .collect();
        let deleted: Vec<_> = diff
            .iter()
            .filter(|l| l.kind == DiffKind::Del)
            .map(|l| l.content.as_str())
            .collect();
        assert!(added.contains(&"TWO"), "added={added:?}");
        assert!(added.contains(&"three"), "added={added:?}");
        assert!(deleted.contains(&"two"), "deleted={deleted:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn collect_diff_lines(diff: &Diff) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let _ = diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        let content = String::from_utf8_lossy(line.content())
            .trim_end_matches(['\r', '\n'])
            .to_string();
        let kind = match line.origin() {
            '+' => DiffKind::Add,
            '-' => DiffKind::Del,
            ' ' => DiffKind::Context,
            'H' => DiffKind::Hunk,
            // 'F'（ファイルヘッダ）など差分本文以外は捨てる
            _ => return true,
        };
        lines.push(DiffLine {
            kind,
            old_lineno: line.old_lineno(),
            new_lineno: line.new_lineno(),
            content,
        });
        true
    });
    lines
}
