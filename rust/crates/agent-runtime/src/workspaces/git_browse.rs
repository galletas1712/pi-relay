//! Browse-time git status and diffs for session workspace roots.
//!
//! Status is repo-wide (independent of lazy `list_dir` expansion). Paths are
//! always cwd-relative, prefixed with `workspace_dir/`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use agent_runtime_protocol::{
    GitAgainst, GitBrowseRoot, GitComparison, GitComparisonRef, GitFileStatus, GitPullRequest,
    GitStatusEntry, GitStatusRoot,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;

const DIFF_BYTE_CAP: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusReport {
    pub against: GitAgainst,
    pub roots: Vec<GitStatusRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffReport {
    pub path: String,
    pub against: GitAgainst,
    pub comparison: Option<GitComparison>,
    pub status: Option<GitFileStatus>,
    pub unified: String,
    pub binary: bool,
    pub truncated: bool,
}

pub fn git_status(
    cwd: &Path,
    roots: &[GitBrowseRoot],
    against: GitAgainst,
) -> Result<GitStatusReport> {
    let mut out = Vec::with_capacity(roots.len());
    for root in roots {
        out.push(status_for_root(cwd, root, against));
    }
    Ok(GitStatusReport {
        against,
        roots: out,
    })
}

pub fn git_diff(
    cwd: &Path,
    path: &str,
    roots: &[GitBrowseRoot],
    against: GitAgainst,
) -> Result<GitDiffReport> {
    let path = normalize_cwd_rel(path)?;
    let Some(root) = owning_root(&path, roots) else {
        bail!("path is not under a git workspace root: {path}");
    };
    let repo = cwd.join(&root.workspace_dir);
    if !repo.join(".git").exists() {
        bail!(
            "workspace root is not a git repository: {}",
            root.workspace_dir
        );
    }
    let rel_in_repo = path_relative_to_root(&path, &root.workspace_dir)?;
    let comparison = comparison_for_root(&repo, root, against)?;
    let base_arg = comparison
        .as_ref()
        .map(|value| value.merge_base_oid.as_str())
        .unwrap_or("HEAD");

    let status_map = status_map_for_root(cwd, root, against, base_arg)?;
    let status = status_map.get(&path).copied();

    let (unified, binary, truncated) = if status == Some(GitFileStatus::Untracked) {
        // Untracked files are absent from a normal git diff.
        diff_as_new_file(&repo, &rel_in_repo)?
    } else {
        diff_against(&repo, &base_arg, &rel_in_repo)?
    };

    Ok(GitDiffReport {
        path,
        against,
        comparison,
        status,
        unified,
        binary,
        truncated,
    })
}

fn status_for_root(cwd: &Path, root: &GitBrowseRoot, against: GitAgainst) -> GitStatusRoot {
    let result = (|| {
        let repo = cwd.join(&root.workspace_dir);
        let comparison = comparison_for_root(&repo, root, against)?;
        let base_arg = comparison
            .as_ref()
            .map(|value| value.merge_base_oid.as_str())
            .unwrap_or("HEAD");
        let map = status_map_for_root(cwd, root, against, base_arg)?;
        Ok::<_, anyhow::Error>((comparison, map))
    })();
    match result {
        Ok((comparison, map)) => {
            let entries = map
                .into_iter()
                .map(|(path, status)| GitStatusEntry { path, status })
                .collect();
            GitStatusRoot {
                workspace_dir: root.workspace_dir.clone(),
                comparison,
                error: None,
                entries,
            }
        }
        Err(error) => GitStatusRoot {
            workspace_dir: root.workspace_dir.clone(),
            comparison: None,
            error: Some(format!("{error:#}")),
            entries: Vec::new(),
        },
    }
}

fn status_map_for_root(
    cwd: &Path,
    root: &GitBrowseRoot,
    against: GitAgainst,
    base_arg: &str,
) -> Result<BTreeMap<String, GitFileStatus>> {
    let repo = cwd.join(&root.workspace_dir);
    if !repo.join(".git").exists() {
        bail!("not a git repository");
    }
    let mut map = BTreeMap::new();
    match against {
        GitAgainst::WorkingTree => {
            for (rel, status) in
                parse_porcelain(&git_output(&repo, &["status", "--porcelain=v1", "-z"])?)
            {
                map.insert(cwd_join(&root.workspace_dir, &rel), status);
            }
        }
        GitAgainst::Branch => {
            for (rel, status) in parse_name_status(&git_output(
                &repo,
                &["diff", "--name-status", "-z", base_arg],
            )?) {
                map.insert(cwd_join(&root.workspace_dir, &rel), status);
            }
            // Untracked + conflicts still come from porcelain.
            for (rel, status) in
                parse_porcelain(&git_output(&repo, &["status", "--porcelain=v1", "-z"])?)
            {
                if matches!(status, GitFileStatus::Untracked | GitFileStatus::Conflict) {
                    map.insert(cwd_join(&root.workspace_dir, &rel), status);
                }
            }
        }
    }
    Ok(map)
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    number: u64,
    title: String,
    url: String,
    head_ref_name: String,
    head_ref_oid: String,
    head_repository_owner: GhRepositoryOwner,
    base_ref_name: String,
    base_ref_oid: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct GhRepositoryOwner {
    login: String,
}

fn comparison_for_root(
    repo: &Path,
    root: &GitBrowseRoot,
    against: GitAgainst,
) -> Result<Option<GitComparison>> {
    if against == GitAgainst::WorkingTree {
        return Ok(None);
    }

    let head_oid = git_output(repo, &["rev-parse", "HEAD"])?;
    let current_branch = git_output(repo, &["symbolic-ref", "--short", "HEAD"])
        .unwrap_or_else(|_| root.local_branch.clone());
    let fallback_base_oid = git_output(
        repo,
        &[
            "rev-parse",
            &format!("refs/remotes/origin/{}", root.remote_branch),
        ],
    )?;

    let mut base = GitComparisonRef {
        branch: root.remote_branch.clone(),
        oid: fallback_base_oid,
        pull_request: None,
    };
    let mut tip = GitComparisonRef {
        branch: current_branch,
        oid: head_oid.clone(),
        pull_request: None,
    };

    if let Some((owner, repository)) = github_repo(&root.remote_url) {
        if let Some(pull_requests) = github_pull_requests(repo, &owner, &repository) {
            if let Some((tip_pr, base_pr)) = resolve_pr_pair(
                &pull_requests,
                &owner,
                &root.remote_branch,
                &head_oid,
                |oid| git_is_ancestor(repo, oid, "HEAD"),
            ) {
                base = GitComparisonRef {
                    branch: tip_pr.base_ref_name.clone(),
                    oid: tip_pr.base_ref_oid.clone(),
                    pull_request: base_pr.map(pull_request_view),
                };
                tip = GitComparisonRef {
                    branch: tip_pr.head_ref_name.clone(),
                    oid: head_oid,
                    pull_request: Some(pull_request_view(tip_pr)),
                };
            }
        }
    }

    let merge_base_oid = git_output(repo, &["merge-base", "HEAD", &base.oid])?;
    Ok(Some(GitComparison {
        base,
        tip,
        merge_base_oid,
    }))
}

fn resolve_pr_pair<'a>(
    pull_requests: &'a [GhPullRequest],
    repository_owner: &str,
    remote_branch: &str,
    head_oid: &str,
    mut is_ancestor: impl FnMut(&str) -> bool,
) -> Option<(&'a GhPullRequest, Option<&'a GhPullRequest>)> {
    let tip = pull_requests
        .iter()
        .find(|pr| pr.head_ref_oid == head_oid)
        .or_else(|| {
            pull_requests.iter().find(|pr| {
                pr.head_repository_owner.login == repository_owner
                    && pr.head_ref_name == remote_branch
                    && is_ancestor(&pr.head_ref_oid)
            })
        })?;
    let base = pull_requests
        .iter()
        .find(|pr| pr.head_ref_name == tip.base_ref_name && pr.head_ref_oid == tip.base_ref_oid)
        .or_else(|| {
            pull_requests.iter().find(|pr| {
                pr.head_repository_owner.login == repository_owner
                    && pr.head_ref_name == tip.base_ref_name
            })
        });
    Some((tip, base))
}

fn pull_request_view(pr: &GhPullRequest) -> GitPullRequest {
    GitPullRequest {
        number: pr.number,
        title: pr.title.clone(),
        url: pr.url.clone(),
    }
}

fn github_pull_requests(repo: &Path, owner: &str, repository: &str) -> Option<Vec<GhPullRequest>> {
    let slug = format!("{owner}/{repository}");
    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--repo",
            &slug,
            "--state",
            "open",
            "--limit",
            "500",
            "--json",
            "number,title,url,headRefName,headRefOid,headRepositoryOwner,baseRefName,baseRefOid",
        ])
        .current_dir(repo)
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .ok()?;
    decode_github_pull_requests(output.status.success(), &output.stdout)
}

fn decode_github_pull_requests(success: bool, stdout: &[u8]) -> Option<Vec<GhPullRequest>> {
    if !success {
        return None;
    }
    serde_json::from_slice(stdout).ok()
}

fn github_repo(remote_url: &str) -> Option<(String, String)> {
    let path = remote_url
        .strip_prefix("https://github.com/")
        .or_else(|| remote_url.strip_prefix("http://github.com/"))
        .or_else(|| remote_url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote_url.strip_prefix("git@github.com:"))?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let (owner, repository) = path.split_once('/')?;
    if owner.is_empty() || repository.is_empty() || repository.contains('/') {
        return None;
    }
    Some((owner.to_string(), repository.to_string()))
}

fn git_is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    git_command()
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(repo)
        .status()
        .is_ok_and(|status| status.success())
}

fn diff_against(repo: &Path, base: &str, rel: &str) -> Result<(String, bool, bool)> {
    let output = git_command()
        .args(["diff", "--no-color", base, "--", rel])
        .current_dir(repo)
        .output()
        .with_context(|| format!("git diff in {}", repo.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("does not exist") || lower.contains("no such path") {
            return Ok((String::new(), false, false));
        }
        bail!("git diff failed: {}", stderr.trim());
    }
    Ok(cap_diff_output(&output.stdout))
}

fn diff_as_new_file(repo: &Path, rel: &str) -> Result<(String, bool, bool)> {
    let abs = repo.join(rel);
    if !abs.is_file() {
        return Ok((String::new(), false, false));
    }
    let output = git_command()
        .args(["diff", "--no-color", "--no-index", "--", "/dev/null", rel])
        .current_dir(repo)
        .output()
        .with_context(|| format!("git diff --no-index in {}", repo.display()))?;
    // --no-index returns 1 when files differ; that is success for our purposes.
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "git diff --no-index failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(cap_diff_output(&output.stdout))
}

fn cap_diff_output(stdout: &[u8]) -> (String, bool, bool) {
    let binary = stdout.windows(12).any(|w| w == b"Binary files")
        || stdout.windows(14).any(|w| w == b"Binary file(s)");
    let truncated = stdout.len() > DIFF_BYTE_CAP;
    let slice = if truncated {
        &stdout[..DIFF_BYTE_CAP]
    } else {
        stdout
    };
    let unified = String::from_utf8_lossy(slice).into_owned();
    (unified, binary, truncated)
}

fn owning_root<'a>(path: &str, roots: &'a [GitBrowseRoot]) -> Option<&'a GitBrowseRoot> {
    roots
        .iter()
        .filter(|root| {
            path == root.workspace_dir || path.starts_with(&format!("{}/", root.workspace_dir))
        })
        .max_by_key(|root| root.workspace_dir.len())
}

fn path_relative_to_root(path: &str, workspace_dir: &str) -> Result<String> {
    if path == workspace_dir {
        bail!("path must name a file under the workspace root");
    }
    let prefix = format!("{workspace_dir}/");
    path.strip_prefix(&prefix)
        .map(str::to_string)
        .with_context(|| format!("path {path} is not under {workspace_dir}"))
}

fn cwd_join(workspace_dir: &str, rel: &str) -> String {
    if workspace_dir.is_empty() {
        rel.to_string()
    } else {
        format!("{workspace_dir}/{rel}")
    }
}

fn normalize_cwd_rel(path: &str) -> Result<String> {
    let path = path.trim().trim_start_matches('/');
    if path.is_empty() || path.contains('\0') || path.split('/').any(|p| p == ".." || p.is_empty())
    {
        bail!("invalid browse path");
    }
    Ok(path.to_string())
}

/// Parse `git status --porcelain=v1 -z` into (repo-relative path, status).
fn parse_porcelain(raw: &str) -> Vec<(String, GitFileStatus)> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 3 > bytes.len() {
            break;
        }
        let x = bytes[i] as char;
        let y = bytes[i + 1] as char;
        if bytes[i + 2] != b' ' {
            // Unexpected; skip to next NUL.
            if let Some(n) = bytes[i..].iter().position(|&b| b == 0) {
                i += n + 1;
                continue;
            }
            break;
        }
        i += 3;
        let (path, next) = read_z_path(bytes, i);
        i = next;
        // In porcelain -z output, the destination comes before the source.
        let path = if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            let (_source, next2) = read_z_path(bytes, i);
            i = next2;
            path
        } else {
            path
        };
        if path.is_empty() {
            continue;
        }
        out.push((path, porcelain_status(x, y)));
    }
    out
}

fn porcelain_status(x: char, y: char) -> GitFileStatus {
    let pair = [x, y];
    if pair == ['?', '?'] {
        return GitFileStatus::Untracked;
    }
    if "AUD".contains(x) && "AUD".contains(y) && x != y
        || matches!((x, y), ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D'))
    {
        return GitFileStatus::Conflict;
    }
    if x == 'A' || y == 'A' {
        return GitFileStatus::Added;
    }
    if x == 'D' || y == 'D' {
        return GitFileStatus::Deleted;
    }
    if x == '?' || y == '?' {
        return GitFileStatus::Untracked;
    }
    GitFileStatus::Modified
}

/// Parse `git diff --name-status -z <base>`.
fn parse_name_status(raw: &str) -> Vec<(String, GitFileStatus)> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let (status_tok, next) = read_z_path(bytes, i);
        i = next;
        if status_tok.is_empty() {
            break;
        }
        let code = status_tok.chars().next().unwrap_or('M');
        let status = match code {
            'A' => GitFileStatus::Added,
            'D' => GitFileStatus::Deleted,
            'U' => GitFileStatus::Conflict,
            _ => GitFileStatus::Modified,
        };
        if matches!(code, 'R' | 'C') {
            let (_old, next_old) = read_z_path(bytes, i);
            i = next_old;
            let (new_path, next_new) = read_z_path(bytes, i);
            i = next_new;
            if !new_path.is_empty() {
                out.push((new_path, GitFileStatus::Added));
            }
        } else {
            let (path, next_path) = read_z_path(bytes, i);
            i = next_path;
            if !path.is_empty() {
                out.push((path, status));
            }
        }
    }
    out
}

fn read_z_path(bytes: &[u8], start: usize) -> (String, usize) {
    if start >= bytes.len() {
        return (String::new(), start);
    }
    let end = bytes[start..]
        .iter()
        .position(|&b| b == 0)
        .map(|n| start + n)
        .unwrap_or(bytes.len());
    let path = String::from_utf8_lossy(&bytes[start..end]).into_owned();
    let next = if end < bytes.len() { end + 1 } else { end };
    (path, next)
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = git_command()
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("run git in {}", cwd.display()))?;
    if !output.status.success() {
        bail!(
            "git failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "pi-relay")
        .env("GIT_AUTHOR_EMAIL", "pi-relay@example.invalid")
        .env("GIT_COMMITTER_NAME", "pi-relay")
        .env("GIT_COMMITTER_EMAIL", "pi-relay@example.invalid");
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use tempfile::TempDir;

    /// Session cwd containing one git workspace dir named `repo`.
    fn init_session_cwd() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().expect("tempdir");
        let seed = tmp.path().join("seed");
        let origin = tmp.path().join("origin.git");
        let cwd = tmp.path().join("cwd");
        let repo = cwd.join("repo");
        fs::create_dir_all(&seed).unwrap();
        fs::create_dir_all(&cwd).unwrap();

        git(&seed, &["init", "-b", "main"]);
        git(&seed, &["config", "user.email", "t@example.com"]);
        git(&seed, &["config", "user.name", "t"]);
        git(&seed, &["config", "commit.gpgsign", "false"]);
        fs::write(seed.join("README.md"), "base\n").unwrap();
        fs::create_dir_all(seed.join("src/deep")).unwrap();
        fs::write(seed.join("src/deep/nested.rs"), "fn a() {}\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-m", "init"]);
        git(
            tmp.path(),
            &[
                "clone",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        git(
            &cwd,
            &["clone", origin.to_str().unwrap(), repo.to_str().unwrap()],
        );
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(&repo, &["checkout", "-b", "feature"]);
        git(&repo, &["fetch", "origin", "main"]);
        (tmp, cwd)
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn root() -> GitBrowseRoot {
        GitBrowseRoot {
            workspace_dir: "repo".into(),
            remote_url: "file:///tmp/origin.git".into(),
            remote_branch: "main".into(),
            local_branch: "feature".into(),
        }
    }

    #[test]
    fn working_tree_status_marks_modified_and_untracked() {
        let (_tmp, cwd) = init_session_cwd();
        let repo = cwd.join("repo");
        fs::write(repo.join("README.md"), "changed\n").unwrap();
        fs::write(repo.join("new.txt"), "hi\n").unwrap();

        let report = git_status(&cwd, &[root()], GitAgainst::WorkingTree).unwrap();
        let by_path: BTreeMap<_, _> = report.roots[0]
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.status))
            .collect();
        assert_eq!(
            by_path.get("repo/README.md"),
            Some(&GitFileStatus::Modified)
        );
        assert_eq!(by_path.get("repo/new.txt"), Some(&GitFileStatus::Untracked));
    }

    #[test]
    fn status_resolves_multiple_roots_independently() {
        let (_tmp, cwd) = init_session_cwd();
        git(&cwd, &["clone", "repo", "repo-b"]);
        fs::write(cwd.join("repo/README.md"), "repo a\n").unwrap();
        fs::write(cwd.join("repo-b/README.md"), "repo b\n").unwrap();
        let mut second = root();
        second.workspace_dir = "repo-b".into();

        let report = git_status(&cwd, &[root(), second], GitAgainst::WorkingTree).unwrap();

        assert_eq!(report.roots.len(), 2);
        assert_eq!(report.roots[0].entries[0].path, "repo/README.md");
        assert_eq!(report.roots[1].entries[0].path, "repo-b/README.md");
    }

    #[test]
    fn clean_file_has_no_diff() {
        let (_tmp, cwd) = init_session_cwd();

        let report = git_diff(&cwd, "repo/README.md", &[root()], GitAgainst::WorkingTree).unwrap();

        assert_eq!(report.status, None);
        assert!(report.unified.is_empty());
        assert!(!report.binary);
    }

    #[test]
    fn branch_status_includes_committed_and_uncommitted() {
        let (_tmp, cwd) = init_session_cwd();
        let repo = cwd.join("repo");

        fs::write(repo.join("src/deep/nested.rs"), "fn a() { /* edited */ }\n").unwrap();
        git(&repo, &["add", "src/deep/nested.rs"]);
        git(&repo, &["commit", "-m", "edit nested"]);
        fs::write(repo.join("extra.rs"), "extra\n").unwrap();

        let report = git_status(&cwd, &[root()], GitAgainst::Branch).unwrap();
        let status_root = &report.roots[0];
        assert!(status_root.error.is_none(), "{:?}", status_root.error);
        let comparison = status_root.comparison.as_ref().unwrap();
        assert_eq!(comparison.base.branch, "main");
        assert_eq!(comparison.tip.branch, "feature");
        assert!(comparison.base.pull_request.is_none());
        assert!(comparison.tip.pull_request.is_none());
        let by_path: BTreeMap<_, _> = status_root
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.status))
            .collect();
        assert_eq!(
            by_path.get("repo/src/deep/nested.rs"),
            Some(&GitFileStatus::Modified)
        );
        assert_eq!(
            by_path.get("repo/extra.rs"),
            Some(&GitFileStatus::Untracked)
        );
    }

    fn gh_pr(
        number: u64,
        head_ref_name: &str,
        head_ref_oid: &str,
        base_ref_name: &str,
        base_ref_oid: &str,
    ) -> GhPullRequest {
        GhPullRequest {
            number,
            title: format!("PR {number}"),
            url: format!("https://github.com/example/repo/pull/{number}"),
            head_ref_name: head_ref_name.into(),
            head_ref_oid: head_ref_oid.into(),
            head_repository_owner: GhRepositoryOwner {
                login: "example".into(),
            },
            base_ref_name: base_ref_name.into(),
            base_ref_oid: base_ref_oid.into(),
        }
    }

    #[test]
    fn resolves_stacked_base_and_tip_prs() {
        let prs = [
            gh_pr(1, "stack-base", "base-oid", "main", "main-oid"),
            gh_pr(2, "stack-tip", "tip-oid", "stack-base", "base-oid"),
        ];

        let (tip, base) =
            resolve_pr_pair(&prs, "example", "stack-tip", "local-oid", |_| true).unwrap();

        assert_eq!(tip.number, 2);
        assert_eq!(base.unwrap().number, 1);
    }

    #[test]
    fn exact_head_oid_resolves_pr_when_branch_name_differs() {
        let prs = [gh_pr(2, "published-name", "tip-oid", "main", "main-oid")];

        let (tip, base) =
            resolve_pr_pair(&prs, "example", "local-name", "tip-oid", |_| false).unwrap();

        assert_eq!(tip.number, 2);
        assert!(base.is_none());
    }

    #[test]
    fn unresolved_pr_context_falls_back_to_branches() {
        let prs = [gh_pr(2, "other", "other-oid", "main", "main-oid")];

        assert!(resolve_pr_pair(&prs, "example", "feature", "head-oid", |_| false).is_none());
    }

    #[test]
    fn parses_supported_github_remote_urls() {
        assert_eq!(
            github_repo("https://github.com/example/repo.git"),
            Some(("example".into(), "repo".into()))
        );
        assert_eq!(
            github_repo("git@github.com:example/repo.git"),
            Some(("example".into(), "repo".into()))
        );
        assert_eq!(github_repo("https://gitlab.com/example/repo.git"), None);
    }

    #[test]
    fn unavailable_or_malformed_github_metadata_is_ignored() {
        assert_eq!(decode_github_pull_requests(false, b"[]"), None);
        assert_eq!(decode_github_pull_requests(true, b"not json"), None);
    }

    #[test]
    fn parse_porcelain_handles_rename_records() {
        let raw = "R  new.txt\0old.txt\0";
        let parsed = parse_porcelain(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "new.txt");
        assert_eq!(parsed[0].1, GitFileStatus::Modified);
    }

    #[test]
    fn owning_root_picks_longest_prefix() {
        let roots = [
            GitBrowseRoot {
                workspace_dir: "a".into(),
                remote_url: "https://github.com/example/a.git".into(),
                remote_branch: "main".into(),
                local_branch: "feature".into(),
            },
            GitBrowseRoot {
                workspace_dir: "a/nested".into(),
                remote_url: "https://github.com/example/nested.git".into(),
                remote_branch: "main".into(),
                local_branch: "feature".into(),
            },
        ];
        assert_eq!(
            owning_root("a/nested/file.rs", &roots)
                .unwrap()
                .workspace_dir,
            "a/nested"
        );
    }
}
