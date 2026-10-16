//! Git 操作封装：调用系统 git CLI（无额外原生依赖，兼容任何仓库配置）

use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// CREATE_NO_WINDOW：GUI 进程拉起控制台程序时，防止一闪而过的命令行窗口
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn silent_command(program: &str) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

#[derive(Debug, Serialize, Clone)]
pub struct GitResult {
    pub ok: bool,
    pub output: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct GitStatus {
    pub ok: bool,
    pub branch: String,
    pub ahead: i32,
    pub behind: i32,
    pub dirty: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub current: bool,
}

fn run_git(repo: &Path, args: &[&str]) -> GitResult {
    let out = silent_command("git")
        .args(args)
        .current_dir(repo)
        .output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            GitResult {
                ok: o.status.success(),
                output: if stdout.is_empty() { stderr } else { stdout },
            }
        }
        Err(e) => GitResult { ok: false, output: format!("无法执行 git: {e}") },
    }
}

fn is_repo(repo: &Path) -> Result<(), String> {
    if !repo.exists() {
        return Err(format!("路径不存在: {}", repo.display()));
    }
    let r = run_git(repo, &["rev-parse", "--is-inside-work-tree"]);
    if !r.ok {
        return Err(format!("不是 git 仓库: {}", r.output));
    }
    Ok(())
}

pub fn status(repo: &str) -> GitStatus {
    let path = Path::new(repo);
    if let Err(e) = is_repo(path) {
        return GitStatus { ok: false, branch: String::new(), ahead: 0, behind: 0, dirty: false, error: Some(e) };
    }
    let r = run_git(path, &["status", "--porcelain=v2", "--branch"]);
    if !r.ok {
        return GitStatus { ok: false, branch: String::new(), ahead: 0, behind: 0, dirty: false, error: Some(r.output) };
    }
    let mut branch = String::new();
    let mut ahead = 0;
    let mut behind = 0;
    let mut dirty = false;
    for line in r.output.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            branch = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            // 形如 "+2 -1"
            for tok in rest.split_whitespace() {
                if let Some(n) = tok.strip_prefix('+') {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = tok.strip_prefix('-') {
                    behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("? ") || line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            dirty = true;
        }
    }
    GitStatus { ok: true, branch, ahead, behind, dirty, error: None }
}

pub fn branches(repo: &str) -> Vec<BranchInfo> {
    let path = Path::new(repo);
    if is_repo(path).is_err() {
        return vec![];
    }
    let cur = run_git(path, &["branch", "--show-current"]);
    let current = cur.output.trim().to_string();
    let r = run_git(path, &["branch", "--format=%(refname:short)"]);
    if !r.ok {
        return vec![];
    }
    r.output
        .lines()
        .map(|l| {
            let name = l.trim().to_string();
            BranchInfo { current: name == current, name }
        })
        .collect()
}

pub fn switch_branch(repo: &str, branch: &str) -> GitResult {
    let path = Path::new(repo);
    if let Err(e) = is_repo(path) {
        return GitResult { ok: false, output: e };
    }
    // 优先 git switch，失败回退 checkout
    let r = run_git(path, &["switch", branch]);
    if r.ok { r } else { run_git(path, &["checkout", branch]) }
}

pub fn pull(repo: &str) -> GitResult {
    let path = Path::new(repo);
    if let Err(e) = is_repo(path) {
        return GitResult { ok: false, output: e };
    }
    run_git(path, &["pull", "--rebase", "--autostash"])
}

pub fn push(repo: &str) -> GitResult {
    let path = Path::new(repo);
    if let Err(e) = is_repo(path) {
        return GitResult { ok: false, output: e };
    }
    run_git(path, &["push"])
}

/// 最近 n 条提交（oneline）
pub fn log(repo: &str, n: u32) -> Vec<String> {
    let path = Path::new(repo);
    if is_repo(path).is_err() {
        return vec![];
    }
    let n = n.clamp(1, 100).to_string();
    let r = run_git(path, &["log", "--oneline", "-n", &n]);
    if r.ok {
        r.output.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
    } else {
        vec![]
    }
}

/// 冲突文件列表
pub fn conflicts(repo: &str) -> Vec<String> {
    let path = Path::new(repo);
    if is_repo(path).is_err() {
        return vec![];
    }
    let r = run_git(path, &["diff", "--name-only", "--diff-filter=U"]);
    if r.ok {
        r.output.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
    } else {
        vec![]
    }
}

/// 将推送的提交：origin/<branch>..HEAD（无 upstream 时返回空）
pub fn outgoing(repo: &str, branch: &str) -> Vec<String> {
    let path = Path::new(repo);
    if is_repo(path).is_err() {
        return vec![];
    }
    let range = format!("origin/{branch}..HEAD");
    let r = run_git(path, &["log", "--oneline", &range]);
    if r.ok {
        r.output.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
    } else {
        vec![]
    }
}

/// 上次拉取改动的文件明细（HEAD@{1} diff --stat）
pub fn last_pull_stat(repo: &str) -> String {
    let path = Path::new(repo);
    if is_repo(path).is_err() {
        return String::new();
    }
    let r = run_git(path, &["diff", "--stat", "HEAD@{1}"]);
    if r.ok {
        r.output
    } else {
        String::new()
    }
}

/// 中止进行中的 rebase / merge
pub fn abort_merge(repo: &str) -> GitResult {
    let path = Path::new(repo);
    if let Err(e) = is_repo(path) {
        return GitResult { ok: false, output: e };
    }
    let r = run_git(path, &["rebase", "--abort"]);
    if r.ok {
        r
    } else {
        run_git(path, &["merge", "--abort"])
    }
}

/// 解决冲突后继续 rebase / merge（core.editor=true 跳过编辑器）
pub fn continue_merge(repo: &str) -> GitResult {
    let path = Path::new(repo);
    if let Err(e) = is_repo(path) {
        return GitResult { ok: false, output: e };
    }
    let r = run_git(path, &["-c", "core.editor=true", "rebase", "--continue"]);
    if r.ok {
        r
    } else {
        run_git(path, &["-c", "core.editor=true", "merge", "--continue"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_repo() -> PathBuf {
        // 并行测试线程必须各自独立目录：tag + pid + 线程名 + 纳秒时间戳
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let thread: String = std::thread::current()
            .name()
            .unwrap_or("t")
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let d = std::env::temp_dir().join(format!(
            "tbp-test-git-{}-{}-{}",
            std::process::id(),
            &thread[..thread.len().min(40)],
            nanos
        ));
        if d.exists() {
            std::fs::remove_dir_all(&d).unwrap();
        }
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = silent_command("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo_with_commit(d: &Path) {
        git(d, &["init", "-b", "main"]);
        std::fs::write(d.join("a.txt"), "hello").unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-m", "init"]);
    }

    fn bare_repo() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("tbp-test-git-bare-{}-{}", std::process::id(), nanos));
        if d.exists() {
            std::fs::remove_dir_all(&d).unwrap();
        }
        d
    }

    fn clone_repo() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("tbp-test-git-clone-{}-{}", std::process::id(), nanos));
        if d.exists() {
            std::fs::remove_dir_all(&d).unwrap();
        }
        d
    }

    #[test]
    fn status_reports_clean_repo() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        let s = status(&d.to_string_lossy());
        assert!(s.ok, "error: {:?}", s.error);
        assert_eq!(s.branch, "main");
        assert!(!s.dirty);
        assert_eq!(s.ahead, 0);
        assert_eq!(s.behind, 0);
    }

    #[test]
    fn status_detects_dirty_and_branch() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        git(&d, &["checkout", "-b", "feature/x"]);
        std::fs::write(d.join("dirty.txt"), "x").unwrap();
        let s = status(&d.to_string_lossy());
        assert!(s.ok);
        assert_eq!(s.branch, "feature/x");
        assert!(s.dirty);
    }

    #[test]
    fn status_rejects_non_repo() {
        let d = tmp_repo();
        let s = status(&d.to_string_lossy());
        assert!(!s.ok);
        assert!(s.error.is_some());
    }

    #[test]
    fn status_rejects_missing_path() {
        let s = status("G:\\no-such-dir-tbp-12345");
        assert!(!s.ok);
    }

    #[test]
    fn branches_lists_all_with_current() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        git(&d, &["branch", "dev"]);
        git(&d, &["branch", "hotfix/y"]);
        git(&d, &["checkout", "dev"]);
        let bs = branches(&d.to_string_lossy());
        let names: Vec<&str> = bs.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"dev"));
        assert!(names.contains(&"hotfix/y"));
        let cur: Vec<_> = bs.iter().filter(|b| b.current).collect();
        assert_eq!(cur.len(), 1);
        assert_eq!(cur[0].name, "dev");
    }

    #[test]
    fn switch_branch_changes_head() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        git(&d, &["branch", "dev"]);
        let r = switch_branch(&d.to_string_lossy(), "dev");
        assert!(r.ok, "{}", r.output);
        let s = status(&d.to_string_lossy());
        assert_eq!(s.branch, "dev");
        // 切回不存在的分支应失败
        let bad = switch_branch(&d.to_string_lossy(), "no-such-branch");
        assert!(!bad.ok);
    }

    #[test]
    fn pull_without_remote_fails_gracefully() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        let r = pull(&d.to_string_lossy());
        // 无 remote，pull 报错但不 panic
        assert!(!r.ok);
        assert!(!r.output.is_empty());
    }

    #[test]
    fn push_without_remote_fails_gracefully() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        let r = push(&d.to_string_lossy());
        assert!(!r.ok);
        assert!(!r.output.is_empty());
    }

    #[test]
    fn clean_repo_has_no_conflicts() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        assert!(conflicts(&d.to_string_lossy()).is_empty());
    }

    #[test]
    fn outgoing_lists_unpushed_commits() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        let bare = bare_repo();
        git(&d.parent().unwrap(), &["init", "--bare", "-b", "main", bare.to_str().unwrap()]);
        git(&d, &["remote", "add", "origin", &bare.to_string_lossy()]);
        git(&d, &["push", "-u", "origin", "main"]);
        // 新提交未推送 → outgoing 1 条
        std::fs::write(d.join("c.txt"), "new").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-m", "unpushed"]);
        let out = outgoing(&d.to_string_lossy(), "main");
        assert_eq!(out.len(), 1, "应有 1 条未推送提交: {:?}", out);
        assert!(out[0].contains("unpushed"));
        // 推送后 → 空
        git(&d, &["push"]);
        assert!(outgoing(&d.to_string_lossy(), "main").is_empty());
    }

    #[test]
    fn last_pull_stat_empty_without_pull() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        // 只有一次提交、无 reflog 变化时可能为空；不应 panic
        last_pull_stat(&d.to_string_lossy());
    }

    #[test]
    fn abort_and_continue_fail_cleanly_on_clean_repo() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        // 干净仓库上没有进行中的合并：应返回失败但不 panic
        assert!(!abort_merge(&d.to_string_lossy()).ok);
        assert!(!continue_merge(&d.to_string_lossy()).ok);
    }

    #[test]
    fn log_lists_commits_newest_first() {
        let d = tmp_repo();
        init_repo_with_commit(&d);
        std::fs::write(d.join("b.txt"), "2").unwrap();
        git(&d, &["add", "."]);
        git(&d, &["commit", "-m", "second-commit-msg"]);
        let lines = log(&d.to_string_lossy(), 10);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("second-commit-msg"), "最新提交应在前: {:?}", lines);
    }

    #[test]
    fn pull_rebase_works_with_local_bare_remote() {
        // 建裸仓库作为 origin，验证 pull --rebase 成功路径
        let d = tmp_repo();
        init_repo_with_commit(&d);
        let bare = bare_repo();
        git(&d.parent().unwrap(), &["init", "--bare", "-b", "main", bare.to_str().unwrap()]);
        git(&d, &["remote", "add", "origin", &bare.to_string_lossy()]);
        git(&d, &["push", "-u", "origin", "main"]);
        // 另一个克隆，提交并推送
        let clone = clone_repo();
        git(&d.parent().unwrap(), &["clone", &bare.to_string_lossy(), &clone.to_string_lossy()]);
        std::fs::write(clone.join("b.txt"), "from clone").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "second"]);
        git(&clone, &["push", "origin", "main"]);
        // 原仓库 pull --rebase 应成功
        let r = pull(&d.to_string_lossy());
        assert!(r.ok, "pull output: {}", r.output);
        assert!(d.join("b.txt").exists(), "pull 后应拿到新文件");
    }
}
