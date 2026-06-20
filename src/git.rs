//! git2 を用いた作業ツリー vs HEAD の状態取得と差分。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use git2::{BranchType, Delta, Diff, DiffFormat, DiffOptions, Oid, Repository, Sort, Status};

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

/// コミット一覧の 1 件（コミットビュー左上ペイン用）。
#[derive(Clone)]
pub struct CommitInfo {
    pub id: Oid,
    /// 短縮ハッシュ（7 桁）。
    pub short: String,
    /// 1 行目のサマリ。
    pub summary: String,
    /// `YYYY-MM-DD`（コミット日時, UTC）。
    pub date: String,
}

/// あるコミットで変更された 1 ファイル。
#[derive(Clone)]
pub struct CommitFile {
    pub rel: String,
    pub status: FileStatus,
}

/// ブランチ切替ピッカーの 1 件。
#[derive(Clone)]
pub struct BranchEntry {
    /// 表示名（ローカルは `feature`、リモートは `origin/feature`）。
    pub display: String,
    /// checkout に渡す名前（リモートは短縮名にして tracking を自動作成させる）。
    pub target: String,
    /// 現在チェックアウト中のブランチか。
    pub is_head: bool,
    /// リモート追跡ブランチか（表示色を変える）。
    pub is_remote: bool,
}

/// `Ctrl-R`（origin の同名ブランチを pull）の結果。
pub enum PullOutcome {
    /// 既に最新（取り込むものが無い）。
    UpToDate,
    /// 取り込み成功（早送り or マージ）。
    Pulled,
    /// コンフリクトのため中止し、作業ツリーを復元した。
    Conflict,
    /// upstream 無し / detached / origin に同名が無い → 呼び出し側は通常 reload にフォールバック。
    NoUpstream,
    /// その他の失敗（オフライン・作業ツリー汚れ等）。stderr を保持。
    Failed(String),
}

/// 任意ブランチを現在ブランチへ merge した結果。
pub enum MergeOutcome {
    UpToDate,
    Merged,
    /// コンフリクトのため中止し、作業ツリーを復元した。
    Conflict,
    Failed(String),
}

/// `git merge` / `git pull`（＝fetch+merge）の共通実行結果。
enum MergeRun {
    UpToDate,
    Done,
    Conflict,
    Failed(String),
}

/// コミット一覧の取得結果。
pub struct CommitLog {
    pub commits: Vec<CommitInfo>,
    /// 現在 HEAD がデフォルトブランチか（true なら全履歴を出している）。
    pub on_default: bool,
    /// 基準となるデフォルトブランチ名（表示用）。
    pub default_branch: String,
}

/// デフォルトブランチとの差分（PR 単位）の取得結果。
pub struct BranchDiff {
    /// merge-base(default, HEAD) → HEAD で変更されたファイル一覧。
    pub files: Vec<CommitFile>,
    /// 基準となるデフォルトブランチ名（表示用）。
    pub default_branch: String,
    /// HEAD がデフォルトと同一（マージベース==HEAD）で差分が無いか。
    pub on_default: bool,
}

/// 全履歴表示時のコミット数上限（巨大リポジトリでの一括構築を避ける）。
const MAX_COMMITS: usize = 2000;

/// 表示用 diff のコンテキスト行数。ファイル全体を文脈として出し、変更点から
/// 離れた未変更行も省略しない（hunk の畳み込みをしない）。libgit2 は実質「全行」扱い。
const FULL_FILE_CONTEXT: u32 = u32::MAX;

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
            .context_lines(FULL_FILE_CONTEXT)
            // pathspec をグロブでなく完全一致にする（名前に *?[] を含むファイル対策）。
            .disable_pathspec_match(true)
            .pathspec(rel.to_string_lossy().to_string());

        let diff = self
            .repo
            .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))
            .ok()?;

        Some(collect_diff_lines(&diff))
    }

    /// 現在のブランチ名（detached HEAD 等で取れなければ `None`）。
    pub fn current_branch(&self) -> Option<String> {
        let head = self.repo.head().ok()?;
        if head.is_branch() {
            head.shorthand().ok().map(|s| s.to_string())
        } else {
            None
        }
    }

    /// リポジトリのデフォルトブランチ名を推定する。
    /// `origin/HEAD` の指す先 → ローカル `main`/`master` の順。
    pub fn default_branch(&self) -> String {
        if let Ok(r) = self.repo.find_reference("refs/remotes/origin/HEAD")
            && let Ok(Some(target)) = r.symbolic_target()
            && let Some(name) = target.strip_prefix("refs/remotes/origin/")
        {
            return name.to_string();
        }
        for cand in ["main", "master"] {
            if self
                .repo
                .find_branch(cand, git2::BranchType::Local)
                .is_ok()
            {
                return cand.to_string();
            }
        }
        "main".to_string()
    }

    /// デフォルトブランチの先端コミット Oid を解決する（ローカル → origin の順）。
    fn default_branch_oid(&self, name: &str) -> Option<Oid> {
        for spec in [name.to_string(), format!("origin/{name}")] {
            if let Ok(obj) = self.repo.revparse_single(&spec)
                && let Ok(commit) = obj.peel_to_commit()
            {
                return Some(commit.id());
            }
        }
        None
    }

    /// コミット一覧を返す。現在ブランチがデフォルトでなければ `default..HEAD`
    /// （デフォルトに無い＝このブランチ固有のコミット）、デフォルトなら全履歴。
    pub fn commit_log(&self) -> CommitLog {
        let default_branch = self.default_branch();
        let mut out = CommitLog {
            commits: Vec::new(),
            on_default: true,
            default_branch: default_branch.clone(),
        };
        let Ok(head) = self.repo.head().and_then(|h| h.peel_to_commit()) else {
            return out;
        };
        let default_oid = self.default_branch_oid(&default_branch);
        let on_default = self.current_branch().as_deref() == Some(default_branch.as_str())
            || default_oid == Some(head.id());
        out.on_default = on_default;

        let Ok(mut walk) = self.repo.revwalk() else {
            return out;
        };
        let _ = walk.set_sorting(Sort::TIME);
        if walk.push(head.id()).is_err() {
            return out;
        }
        // デフォルトに含まれるコミットを隠す（このブランチ固有のものだけ残す）。
        if !on_default && let Some(base) = default_oid {
            let _ = walk.hide(base);
        }

        for oid in walk.flatten().take(MAX_COMMITS) {
            let Ok(commit) = self.repo.find_commit(oid) else {
                continue;
            };
            out.commits.push(CommitInfo {
                id: oid,
                short: oid.to_string().chars().take(7).collect(),
                summary: commit.summary().ok().flatten().unwrap_or("").to_string(),
                date: fmt_date(commit.time().seconds()),
            });
        }
        out
    }

    /// 指定コミットで変更されたファイル一覧（親との差分）。
    pub fn commit_files(&self, id: Oid) -> Vec<CommitFile> {
        let Ok(commit) = self.repo.find_commit(id) else {
            return Vec::new();
        };
        let tree = commit.tree().ok();
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
        let Ok(diff) =
            self.repo
                .diff_tree_to_tree(parent_tree.as_ref(), tree.as_ref(), None)
        else {
            return Vec::new();
        };
        let mut files = Vec::new();
        for delta in diff.deltas() {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path());
            let Some(path) = path else { continue };
            files.push(CommitFile {
                rel: path.to_string_lossy().to_string(),
                status: status_of_delta(delta.status()),
            });
        }
        files
    }

    /// 指定コミットの 1 ファイル分の差分（親 vs コミット）。
    pub fn commit_file_diff(&self, id: Oid, rel: &str) -> Vec<DiffLine> {
        let Ok(commit) = self.repo.find_commit(id) else {
            return Vec::new();
        };
        let tree = commit.tree().ok();
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
        let mut opts = DiffOptions::new();
        opts.context_lines(FULL_FILE_CONTEXT)
            .disable_pathspec_match(true)
            .pathspec(rel);
        let Ok(diff) = self.repo.diff_tree_to_tree(
            parent_tree.as_ref(),
            tree.as_ref(),
            Some(&mut opts),
        ) else {
            return Vec::new();
        };
        collect_diff_lines(&diff)
    }

    /// 指定コミット時点でのファイル内容（差分の「新」側ハイライト用）。
    /// そのコミットで削除された等で存在しなければ空文字列。
    pub fn commit_file_new_content(&self, id: Oid, rel: &str) -> String {
        let blob = self
            .repo
            .find_commit(id)
            .ok()
            .and_then(|c| c.tree().ok())
            .and_then(|t| t.get_path(Path::new(rel)).ok())
            .and_then(|e| e.to_object(&self.repo).ok())
            .and_then(|o| o.peel_to_blob().ok());
        match blob {
            Some(b) => String::from_utf8_lossy(b.content()).into_owned(),
            None => String::new(),
        }
    }

    /// デフォルトブランチとの差分（PR 単位）の基準を解決する。
    /// 返り値: `(マージベース Oid, HEAD Oid, デフォルトブランチ名, on_default)`。
    /// `on_default` は HEAD がマージベースと同一＝デフォルトとの固有差分が無いとき true。
    fn branch_base(&self) -> Option<(Oid, Oid, String, bool)> {
        let head = self.repo.head().ok()?.peel_to_commit().ok()?.id();
        let default = self.default_branch();
        let default_oid = self.default_branch_oid(&default)?;
        // マージベースが取れない（無関係な履歴）ときはデフォルト先端を基準にする。
        let base = self
            .repo
            .merge_base(head, default_oid)
            .unwrap_or(default_oid);
        let on_default = base == head;
        Some((base, head, default, on_default))
    }

    /// デフォルトブランチとの差分で変更されたファイル一覧（merge-base→HEAD）。
    pub fn branch_diff(&self) -> BranchDiff {
        let Some((base, head, default_branch, on_default)) = self.branch_base() else {
            return BranchDiff {
                files: Vec::new(),
                default_branch: self.default_branch(),
                on_default: true,
            };
        };
        let mut out = BranchDiff {
            files: Vec::new(),
            default_branch,
            on_default,
        };
        if on_default {
            return out; // 固有差分が無い（デフォルト上、または分岐していない）。
        }
        let base_tree = self.repo.find_commit(base).ok().and_then(|c| c.tree().ok());
        let head_tree = self.repo.find_commit(head).ok().and_then(|c| c.tree().ok());
        let Ok(diff) =
            self.repo
                .diff_tree_to_tree(base_tree.as_ref(), head_tree.as_ref(), None)
        else {
            return out;
        };
        for delta in diff.deltas() {
            let path = delta.new_file().path().or_else(|| delta.old_file().path());
            let Some(path) = path else { continue };
            out.files.push(CommitFile {
                rel: path.to_string_lossy().to_string(),
                status: status_of_delta(delta.status()),
            });
        }
        out
    }

    /// デフォルトブランチとの 1 ファイル分の差分（merge-base→HEAD、全行コンテキスト）。
    pub fn branch_file_diff(&self, rel: &str) -> Vec<DiffLine> {
        let Some((base, head, _, _)) = self.branch_base() else {
            return Vec::new();
        };
        let base_tree = self.repo.find_commit(base).ok().and_then(|c| c.tree().ok());
        let head_tree = self.repo.find_commit(head).ok().and_then(|c| c.tree().ok());
        let mut opts = DiffOptions::new();
        opts.context_lines(FULL_FILE_CONTEXT)
            .disable_pathspec_match(true)
            .pathspec(rel);
        let Ok(diff) = self.repo.diff_tree_to_tree(
            base_tree.as_ref(),
            head_tree.as_ref(),
            Some(&mut opts),
        ) else {
            return Vec::new();
        };
        collect_diff_lines(&diff)
    }

    /// HEAD 時点でのファイル内容（差分の「新」側ハイライト用）。
    pub fn branch_file_content(&self, rel: &str) -> String {
        let Some((_, head, _, _)) = self.branch_base() else {
            return String::new();
        };
        self.commit_file_new_content(head, rel)
    }

    /// `origin` から fetch する（ブランチ一覧を出す前に最新化）。成功なら `true`。
    /// 認証・SSH を確実に扱うため git CLI に委譲する（ネットワーク待ちでブロックする）。
    pub fn fetch_origin(&self) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(["fetch", "origin", "--quiet"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// ローカル＋リモート（origin）のブランチ一覧。現在ブランチを先頭、ローカル → リモートの順。
    /// ローカルに同名がある origin ブランチと `origin/HEAD` は除外する。
    pub fn branches(&self) -> Vec<BranchEntry> {
        let mut out = Vec::new();
        if let Ok(iter) = self.repo.branches(Some(BranchType::Local)) {
            for (branch, _) in iter.flatten() {
                if let Ok(Some(name)) = branch.name() {
                    out.push(BranchEntry {
                        display: name.to_string(),
                        target: name.to_string(),
                        is_head: branch.is_head(),
                        is_remote: false,
                    });
                }
            }
        }
        let local: HashSet<String> = out.iter().map(|e| e.target.clone()).collect();
        if let Ok(iter) = self.repo.branches(Some(BranchType::Remote)) {
            for (branch, _) in iter.flatten() {
                if let Ok(Some(name)) = branch.name() {
                    if name.ends_with("/HEAD") {
                        continue;
                    }
                    let short = name.strip_prefix("origin/").unwrap_or(name);
                    if local.contains(short) {
                        continue;
                    }
                    out.push(BranchEntry {
                        display: name.to_string(),
                        target: short.to_string(),
                        is_head: false,
                        is_remote: true,
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            (!a.is_head, a.is_remote, &a.display).cmp(&(!b.is_head, b.is_remote, &b.display))
        });
        out
    }

    /// 指定ブランチを checkout する（git CLI に委譲）。失敗時は stderr を返す。
    pub fn checkout(&self, target: &str) -> Result<(), String> {
        match Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(["checkout", target])
            .output()
        {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// `Ctrl-R`：origin の同名ブランチを pull（通常 merge）する。upstream が無い等で
    /// 取り込めない場合は `NoUpstream` を返し、呼び出し側がローカル reload にフォールバックする。
    pub fn pull_current(&self) -> PullOutcome {
        let Some(branch) = self.current_branch() else {
            return PullOutcome::NoUpstream; // detached HEAD 等
        };
        // origin に同名ブランチが無ければ pull しない（ローカルだけのブランチ等）。
        if self
            .repo
            .revparse_single(&format!("origin/{branch}"))
            .is_err()
        {
            return PullOutcome::NoUpstream;
        }
        match self.run_merge_like(&["pull", "origin", &branch]) {
            MergeRun::UpToDate => PullOutcome::UpToDate,
            MergeRun::Done => PullOutcome::Pulled,
            MergeRun::Conflict => PullOutcome::Conflict,
            MergeRun::Failed(e) => PullOutcome::Failed(e),
        }
    }

    /// 任意の ref（`origin/feature` / `feature`）を現在ブランチへ merge する（git CLI 委譲）。
    pub fn merge(&self, refname: &str) -> MergeOutcome {
        match self.run_merge_like(&["merge", refname]) {
            MergeRun::UpToDate => MergeOutcome::UpToDate,
            MergeRun::Done => MergeOutcome::Merged,
            MergeRun::Conflict => MergeOutcome::Conflict,
            MergeRun::Failed(e) => MergeOutcome::Failed(e),
        }
    }

    /// `git merge` / `git pull` を実行し結果を判定する。コンフリクト検出時は
    /// `git merge --abort` で merge 前の状態へ戻す（srev は編集機能を持たないため）。
    fn run_merge_like(&self, args: &[&str]) -> MergeRun {
        // 実行前の HEAD を控える（成功時に「最新で変化なし」を出力文字列に頼らず判定する）。
        let head_before = self.repo.head().ok().and_then(|h| h.target());
        let out = match Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(args)
            .output()
        {
            Ok(o) => o,
            Err(e) => return MergeRun::Failed(e.to_string()),
        };
        if out.status.success() {
            // 成功時は HEAD が動いたかで Done / UpToDate を分ける。
            let head_after = self.repo.head().ok().and_then(|h| h.target());
            return if head_after == head_before {
                MergeRun::UpToDate
            } else {
                MergeRun::Done
            };
        }
        // 失敗時はリポジトリの状態でコンフリクトを判定する（git 出力の翻訳に依存しない）。
        // マージ進行中（MERGE_HEAD あり）か index に未解決があればコンフリクト扱い。
        let conflicted = self.repo.state() == git2::RepositoryState::Merge
            || self
                .repo
                .index()
                .map(|mut i| i.read(true).is_ok() && i.has_conflicts())
                .unwrap_or(false);
        if conflicted {
            // srev は編集機能を持たないため、コンフリクトは取り込まず merge 前へ戻す。
            let _ = Command::new("git")
                .arg("-C")
                .arg(&self.workdir)
                .args(["merge", "--abort"])
                .status();
            return MergeRun::Conflict;
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        MergeRun::Failed(stderr.trim().to_string())
    }
}

/// git2 の差分 Delta 種別を [`FileStatus`] へ。
fn status_of_delta(d: Delta) -> FileStatus {
    match d {
        Delta::Added | Delta::Copied => FileStatus::Added,
        Delta::Deleted => FileStatus::Deleted,
        Delta::Renamed => FileStatus::Renamed,
        _ => FileStatus::Modified,
    }
}

/// epoch 秒（UTC）を `YYYY-MM-DD` に。chrono 非依存（Howard Hinnant の civil_from_days）。
fn fmt_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
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

    #[test]
    fn commit_log_lists_branch_commits_and_file_diff() {
        let dir = std::env::temp_dir().join(format!("srev_clog_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "tester"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        // フィーチャーブランチ固有のコミットを 2 つ作る。
        git(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(&dir, &["commit", "-qam", "add two"]);
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "add b"]);

        let info = GitInfo::discover(&dir).expect("repo");
        let log = info.commit_log();
        assert!(!log.on_default, "feature branch is not default");
        assert_eq!(log.default_branch, "main");
        let summaries: Vec<&str> = log.commits.iter().map(|c| c.summary.as_str()).collect();
        assert!(summaries.contains(&"add two"), "{summaries:?}");
        assert!(summaries.contains(&"add b"), "{summaries:?}");
        assert!(!summaries.contains(&"init"), "init is on main, should be hidden");

        // 「add b」コミットは b.txt を追加している。
        let addb = log.commits.iter().find(|c| c.summary == "add b").unwrap();
        let files = info.commit_files(addb.id);
        assert!(files.iter().any(|f| f.rel == "b.txt" && f.status == FileStatus::Added));
        let diff = info.commit_file_diff(addb.id, "b.txt");
        assert!(diff.iter().any(|l| l.kind == DiffKind::Add && l.content == "new"));
        assert_eq!(info.commit_file_new_content(addb.id, "b.txt"), "new\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_diff_uses_merge_base_and_full_context() {
        let dir = std::env::temp_dir().join(format!("srev_bdiff_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "tester"]);
        // 共通の祖先（マージベース）。a.txt は未変更行を多く持たせる。
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "init"]);

        // feature ブランチで a.txt を一部変更し、b.txt を追加。
        git(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        git(&dir, &["commit", "-qam", "edit a"]);
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "add b"]);

        // 分岐後に main を進める（three-dot 差分には現れないはず）。
        git(&dir, &["checkout", "-q", "main"]);
        std::fs::write(dir.join("c.txt"), "main only\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "main advance"]);
        git(&dir, &["checkout", "-q", "feature"]);

        let info = GitInfo::discover(&dir).expect("repo");
        let bd = info.branch_diff();
        assert_eq!(bd.default_branch, "main");
        assert!(!bd.on_default, "feature has a branch diff");
        let rels: Vec<&str> = bd.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(rels.contains(&"a.txt"), "{rels:?}");
        assert!(rels.contains(&"b.txt"), "{rels:?}");
        // main の分岐後コミットは three-dot 差分に含めない。
        assert!(!rels.contains(&"c.txt"), "c.txt is main-only after divergence: {rels:?}");
        assert!(
            bd.files.iter().any(|f| f.rel == "b.txt" && f.status == FileStatus::Added)
        );

        // b.txt の内容と差分。
        assert_eq!(info.branch_file_content("b.txt"), "new\n");
        let bdiff = info.branch_file_diff("b.txt");
        assert!(bdiff.iter().any(|l| l.kind == DiffKind::Add && l.content == "new"));

        // a.txt は 1 行だけ変わったが、全行コンテキストで未変更行も Context として出る。
        let adiff = info.branch_file_diff("a.txt");
        assert!(adiff.iter().any(|l| l.kind == DiffKind::Add && l.content == "TWO"));
        assert!(adiff.iter().any(|l| l.kind == DiffKind::Del && l.content == "two"));
        for unchanged in ["one", "three", "four"] {
            assert!(
                adiff.iter().any(|l| l.kind == DiffKind::Context && l.content == unchanged),
                "未変更行 {unchanged:?} が省略されず Context として出るべき: {adiff:?}"
            );
        }

        // デフォルトブランチ上では固有差分は無い（on_default）。
        git(&dir, &["checkout", "-q", "main"]);
        let info_main = GitInfo::discover(&dir).expect("repo");
        let bd_main = info_main.branch_diff();
        assert!(bd_main.on_default, "on main there is no branch-specific diff");
        assert!(bd_main.files.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branches_lists_locals_and_checkout_switches() {
        let dir = std::env::temp_dir().join(format!("srev_branch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "tester"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "init"]);
        git(&dir, &["checkout", "-q", "-b", "feature"]);

        let info = GitInfo::discover(&dir).expect("repo");
        let branches = info.branches();
        let names: Vec<&str> = branches.iter().map(|b| b.display.as_str()).collect();
        assert!(names.contains(&"main"), "{names:?}");
        assert!(names.contains(&"feature"), "{names:?}");
        // 現在ブランチ feature が先頭かつ is_head。
        assert!(branches[0].is_head);
        assert_eq!(branches[0].display, "feature");

        // checkout で実際に切り替わる。
        info.checkout("main").expect("checkout main");
        assert_eq!(info.current_branch().as_deref(), Some("main"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pull_current_brings_in_origin_commits() {
        let base = std::env::temp_dir().join(format!("srev_pull_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let origin = base.join("origin.git");
        let a = base.join("a");
        let b = base.join("b");

        // bare origin（既定ブランチ main）と、それを clone した 2 つの作業ツリー。
        git(&base, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        git(&base, &["clone", "-q", origin.to_str().unwrap(), "a"]);
        git(&a, &["config", "user.email", "t@e.com"]);
        git(&a, &["config", "user.name", "t"]);
        std::fs::write(a.join("f.txt"), "one\n").unwrap();
        git(&a, &["add", "."]);
        git(&a, &["commit", "-qm", "init"]);
        git(&a, &["push", "-q", "origin", "main"]);

        git(&base, &["clone", "-q", origin.to_str().unwrap(), "b"]);
        git(&b, &["config", "user.email", "t@e.com"]);
        git(&b, &["config", "user.name", "t"]);

        // a 側が origin/main を進める。
        std::fs::write(a.join("f.txt"), "one\ntwo\n").unwrap();
        git(&a, &["commit", "-qam", "advance"]);
        git(&a, &["push", "-q", "origin", "main"]);

        // b は古い内容。pull_current で取り込む。
        let info = GitInfo::discover(&b).expect("repo b");
        assert_eq!(info.current_branch().as_deref(), Some("main"));
        assert!(matches!(info.pull_current(), PullOutcome::Pulled));
        assert_eq!(std::fs::read_to_string(b.join("f.txt")).unwrap(), "one\ntwo\n");
        // もう一度 pull すると最新（取り込むものが無い）。
        assert!(matches!(info.pull_current(), PullOutcome::UpToDate));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn merge_brings_in_branch_and_aborts_on_conflict() {
        let dir = std::env::temp_dir().join(format!("srev_merge_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@e.com"]);
        git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "base\n").unwrap();
        std::fs::write(dir.join("b.txt"), "base\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "init"]);

        // feature: b.txt のみ変更（main と非衝突）。
        git(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("b.txt"), "from-feature\n").unwrap();
        git(&dir, &["commit", "-qam", "edit b"]);

        // main: a.txt を変更（feature と非衝突）。feature を merge できる。
        git(&dir, &["checkout", "-q", "main"]);
        std::fs::write(dir.join("a.txt"), "from-main\n").unwrap();
        git(&dir, &["commit", "-qam", "edit a"]);

        let info = GitInfo::discover(&dir).expect("repo");
        assert!(matches!(info.merge("feature"), MergeOutcome::Merged));
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "from-feature\n");

        // 衝突を作る: merge 後の main から分岐し、双方が同じ行(b.txt)を別内容に変更。
        git(&dir, &["checkout", "-q", "-b", "other"]);
        std::fs::write(dir.join("b.txt"), "other-change\n").unwrap();
        git(&dir, &["commit", "-qam", "other edits b"]);
        git(&dir, &["checkout", "-q", "main"]);
        std::fs::write(dir.join("b.txt"), "main-change\n").unwrap();
        git(&dir, &["commit", "-qam", "main edits b"]);

        assert!(matches!(info.merge("other"), MergeOutcome::Conflict));
        // abort 済み：作業ツリーは merge 前（main の b.txt）に戻り、MERGE_HEAD は無い。
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "main-change\n");
        assert!(
            GitInfo::discover(&dir)
                .unwrap()
                .repo
                .revparse_single("MERGE_HEAD")
                .is_err(),
            "merge は中止され MERGE_HEAD は残らない"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
