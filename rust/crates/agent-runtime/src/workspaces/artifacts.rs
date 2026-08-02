//! Bounded, read-only inspection of a persisted session workspace.

use std::path::{Component, Path, PathBuf};

use agent_runtime_protocol::{
    ArtifactEntry, ArtifactEntryKind, ArtifactsDiff, ArtifactsFile, ArtifactsSnapshot, GitChange,
    GitSnapshot, SessionWorkspace, WorkspaceKind,
};
use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const MAX_TREE_ENTRIES: usize = 10_000;
const MAX_OUTPUT: usize = 512 * 1024;
const MAX_FILE: usize = 256 * 1024;
const MAX_DIFF: usize = 512 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDOFF_PATHSPEC: &str = ":(exclude).pi-handoff";
const HANDOFF_SUBTREE_PATHSPEC: &str = ":(exclude).pi-handoff/**";

pub async fn snapshot(
    manager: &super::WorkspaceManager,
    workspace_id: &str,
    workspace: &SessionWorkspace,
) -> Result<ArtifactsSnapshot> {
    let root = resolve_declared(manager, workspace_id, workspace).await?;
    let tree = walk_tree(&root).await?;
    let git = if workspace.kind == WorkspaceKind::Git {
        Some(git_snapshot(&root, workspace).await?)
    } else {
        None
    };
    Ok(ArtifactsSnapshot {
        workspace_dir: workspace.workspace_dir.clone(),
        tree,
        git,
    })
}

fn git_status_args() -> Vec<String> {
    [
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--",
        ".",
        HANDOFF_PATHSPEC,
        HANDOFF_SUBTREE_PATHSPEC,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn git_diff_args(baseline: &str, path: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "diff".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        "--no-color".to_string(),
        baseline.to_string(),
        "--".to_string(),
    ];
    if let Some(path) = path {
        args.push(path.to_string());
    }
    args.push(HANDOFF_PATHSPEC.to_string());
    args.push(HANDOFF_SUBTREE_PATHSPEC.to_string());
    args
}

fn valid_baseline(value: Option<&str>) -> Option<&str> {
    value.filter(|sha| {
        (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

pub async fn read_file(
    manager: &super::WorkspaceManager,
    workspace_id: &str,
    workspace_dir: &str,
    rel_path: &str,
) -> Result<ArtifactsFile> {
    let root = resolve_declared_dir(manager, workspace_id, workspace_dir).await?;
    read_file_from_root(&root, rel_path).await
}

async fn read_file_from_root(root: &Path, rel_path: &str) -> Result<ArtifactsFile> {
    let path = safe_relative(root, rel_path)?;
    reject_symlink(root, &path).await?;
    let file = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("open workspace file {rel_path}"))?;
    // Do not use fs::read here: artifact files are user-controlled and their
    // metadata size can change between the tree walk and this request.
    let mut bytes = Vec::with_capacity(MAX_FILE + 1);
    file.take((MAX_FILE + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("read workspace file {rel_path}"))?;
    let truncated = bytes.len() > MAX_FILE;
    bytes.truncate(MAX_FILE);
    if bytes.contains(&0) {
        bail!("binary workspace files are not readable");
    }
    Ok(ArtifactsFile {
        path: rel_path.to_string(),
        contents: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    })
}

pub async fn diff(
    manager: &super::WorkspaceManager,
    workspace_id: &str,
    workspace: &SessionWorkspace,
    rel_path: Option<&str>,
) -> Result<ArtifactsDiff> {
    let root = resolve_declared(manager, workspace_id, workspace).await?;
    let args = if let Some(path) = rel_path {
        let safe_path = safe_relative(&root, path)?;
        reject_symlink(&root, &safe_path).await?;
        if is_untracked(&root, path).await? {
            let file = read_file_from_root(&root, path).await?;
            return Ok(ArtifactsDiff {
                path: Some(path.to_string()),
                truncated: file.truncated,
                contents: format!(
                    "Untracked file; no committed diff exists.\n\n{}",
                    file.contents
                ),
            });
        }
        git_diff_args(
            valid_baseline(workspace.base_sha.as_deref()).unwrap_or("HEAD"),
            Some(path),
        )
    } else {
        git_diff_args(
            valid_baseline(workspace.base_sha.as_deref()).unwrap_or("HEAD"),
            None,
        )
    };
    let output = git_output(&root, args, MAX_DIFF).await?;
    Ok(ArtifactsDiff {
        path: rel_path.map(str::to_string),
        truncated: output.1,
        contents: String::from_utf8_lossy(&output.0).into_owned(),
    })
}

fn git_untracked_args(path: &str) -> Vec<String> {
    [
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--",
        path,
        HANDOFF_PATHSPEC,
        HANDOFF_SUBTREE_PATHSPEC,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

async fn is_untracked(root: &Path, path: &str) -> Result<bool> {
    let output = git_output(root, git_untracked_args(path), 8 * 1024).await?;
    Ok(parse_status(&output.0)
        .into_iter()
        .any(|change| change.path == path && change.status == "??"))
}

async fn resolve_declared(
    manager: &super::WorkspaceManager,
    workspace_id: &str,
    workspace: &SessionWorkspace,
) -> Result<PathBuf> {
    super::validate_workspace_dir(&workspace.workspace_dir)?;
    resolve_declared_dir(manager, workspace_id, &workspace.workspace_dir).await
}

async fn resolve_declared_dir(
    manager: &super::WorkspaceManager,
    workspace_id: &str,
    workspace_dir: &str,
) -> Result<PathBuf> {
    super::validate_workspace_dir(workspace_dir)?;
    if workspace_id.trim().is_empty()
        || workspace_id.contains('/')
        || workspace_id.contains('\\')
        || workspace_id.contains('\0')
    {
        bail!("invalid workspace id");
    }
    // Check both the managed session directory and its cwd. In particular,
    // checking cwd alone would accept sessions/<id> implemented as a symlink.
    manager.ensure_session_owns_cwd(workspace_id).await?;
    let cwd = manager.resolve(workspace_id);
    let metadata = std::fs::symlink_metadata(&cwd)
        .with_context(|| format!("inspect managed workspace {}", cwd.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("managed workspace cwd is not a directory");
    }
    let root = cwd.join(workspace_dir);
    let metadata = std::fs::symlink_metadata(&root)
        .with_context(|| format!("inspect declared workspace {workspace_dir}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("declared workspace is not an owned directory");
    }
    // The checks above and this open-by-path sequence cannot form an atomic
    // no-follow transaction on all supported platforms. A concurrent local
    // attacker could still replace a component after validation; callers run
    // inside the daemon-owned session directory and symlink components are
    // rejected again before file reads.
    Ok(root)
}

fn safe_relative(root: &Path, rel_path: &str) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    let mut count = 0;
    for component in Path::new(rel_path).components() {
        match component {
            Component::Normal(part) => {
                if part.is_empty() || part == ".pi-handoff" || part == ".git" {
                    bail!("workspace artifact path is not readable");
                }
                path.push(part);
                count += 1;
            }
            _ => bail!("workspace artifact path must be relative and normal"),
        }
    }
    if count == 0 {
        bail!("workspace artifact path is required");
    }
    Ok(path)
}

async fn reject_symlink(root: &Path, path: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    let relative = path
        .strip_prefix(root)
        .context("workspace path escaped its root")?;
    for component in relative.components() {
        if let Component::Normal(part) = component {
            current.push(part);
            if tokio::fs::symlink_metadata(&current)
                .await
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false)
            {
                bail!("symlink workspace paths are not readable");
            }
        }
    }
    Ok(())
}

async fn walk_tree(root: &Path) -> Result<Vec<ArtifactEntry>> {
    let mut entries = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let mut reader = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = reader.next_entry().await? {
            if entries.len() >= MAX_TREE_ENTRIES {
                return Ok(entries);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".pi-handoff" || name == ".git" {
                continue;
            }
            let path = entry.path();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let metadata = tokio::fs::symlink_metadata(&path).await?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                entries.push(ArtifactEntry {
                    path: rel.clone(),
                    kind: ArtifactEntryKind::Directory,
                    size: 0,
                });
                stack.push((path, rel));
            } else if metadata.is_file() {
                entries.push(ArtifactEntry {
                    path: rel,
                    kind: ArtifactEntryKind::File,
                    size: metadata.len(),
                });
            }
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

async fn git_snapshot(root: &Path, workspace: &SessionWorkspace) -> Result<GitSnapshot> {
    validate_git_root(root).await?;
    let head = git_text(root, ["rev-parse", "HEAD"]).await.ok();
    let branch = git_text(root, ["branch", "--show-current"]).await.ok();
    let status = git_output(root, git_status_args(), MAX_OUTPUT).await?;
    Ok(GitSnapshot {
        head,
        branch: branch.filter(|value| !value.is_empty()),
        baseline: workspace.base_sha.clone(),
        changes: parse_status(&status.0),
        truncated: status.1,
    })
}

fn parse_status(bytes: &[u8]) -> Vec<GitChange> {
    let parts = bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty());
    let mut changes = Vec::new();
    let mut iter = parts.peekable();
    while let Some(part) = iter.next() {
        let text = String::from_utf8_lossy(part);
        let Some((status, path)) = text.split_at_checked(3) else {
            continue;
        };
        let status = status.trim();
        let path = path.trim();
        if status.is_empty() || path.is_empty() {
            continue;
        }
        let old_path = if status.starts_with('R') || status.starts_with('C') {
            iter.next()
                .map(|value| String::from_utf8_lossy(value).into_owned())
        } else {
            None
        };
        changes.push(GitChange {
            path: path.trim().to_string(),
            status: status.to_string(),
            old_path,
        });
    }
    changes
}

async fn git_text<const N: usize>(root: &Path, args: [&str; N]) -> Result<String> {
    let (bytes, _) = git_output(root, args, 8 * 1024).await?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

async fn validate_git_root(root: &Path) -> Result<()> {
    let git_dir = root.join(".git");
    let metadata = tokio::fs::symlink_metadata(&git_dir)
        .await
        .context("inspect workspace git directory")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("workspace git directory is not a managed directory");
    }
    let canonical_root = tokio::fs::canonicalize(root)
        .await
        .context("canonicalize workspace git root")?;
    let canonical_git = tokio::fs::canonicalize(&git_dir)
        .await
        .context("canonicalize workspace git directory")?;
    if !canonical_git.starts_with(&canonical_root) {
        bail!("workspace git directory escapes its workspace");
    }
    Ok(())
}

async fn git_output<I, S>(root: &Path, args: I, cap: usize) -> Result<(Vec<u8>, bool)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    validate_git_root(root).await?;
    let mut command = Command::new("git");
    command
        .args(["-c", "core.fsmonitor=false", "-c", "diff.external="])
        .args(args)
        .current_dir(root)
        .kill_on_drop(true)
        // Start with no inherited configuration/environment. Besides the
        // commonly abused GIT_DIR/GIT_WORK_TREE/GIT_INDEX_FILE variables,
        // this excludes object-directory and alternate-object variables.
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_DIR", ".git")
        .env("GIT_WORK_TREE", ".")
        .env("GIT_INDEX_FILE", ".git/index")
        .env("GIT_OBJECT_DIRECTORY", ".git/objects")
        .env("GIT_COMMON_DIR", ".git")
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0");
    // The repository-local config is still loaded, but inspection must not
    // allow it to start a fsmonitor or configure a helper process. Diff's
    // external/textconv helpers are disabled in git_diff_args above.
    let output = timeout(GIT_TIMEOUT, bounded_command(command, cap))
        .await
        .context("git inspection timed out")??;
    if !output.status.success() && !output.truncated {
        bail!("git inspection failed");
    }
    Ok((output.stdout, output.truncated))
}

struct BoundedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    truncated: bool,
}

struct BoundedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
) -> std::io::Result<BoundedStream> {
    let mut bytes = Vec::with_capacity(cap.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(BoundedStream {
                bytes,
                truncated: false,
            });
        }
        let remaining = cap.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            return Ok(BoundedStream {
                bytes,
                truncated: true,
            });
        }
    }
}

async fn bounded_command(mut command: Command, cap: usize) -> Result<BoundedOutput> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn git inspection")?;
    let stdout = child.stdout.take().context("capture git stdout")?;
    let stderr = child.stderr.take().context("capture git stderr")?;
    let stdout_task = tokio::spawn(read_bounded(stdout, cap));
    let stderr_task = tokio::spawn(read_bounded(stderr, cap));
    let mut stdout_task = Some(stdout_task);
    let mut stderr_task = Some(stderr_task);
    let mut stdout = None;
    let mut stderr = None;
    let mut status = None;
    let mut killed = false;
    while stdout.is_none() || stderr.is_none() {
        tokio::select! {
            result = stdout_task.as_mut().expect("stdout task"), if stdout_task.is_some() => {
                stdout_task = None;
                let result = result.context("join git stdout")??;
                if result.truncated && !killed {
                    child.kill().await.ok();
                    status = Some(child.wait().await.context("wait for git inspection")?);
                    killed = true;
                }
                stdout = Some(result);
            }
            result = stderr_task.as_mut().expect("stderr task"), if stderr_task.is_some() => {
                stderr_task = None;
                let result = result.context("join git stderr")??;
                if result.truncated && !killed {
                    child.kill().await.ok();
                    status = Some(child.wait().await.context("wait for git inspection")?);
                    killed = true;
                }
                stderr = Some(result);
            }
        }
    }
    let status = match status {
        Some(status) => status,
        None => child.wait().await.context("wait for git inspection")?,
    };
    let stdout = stdout.expect("stdout result");
    let stderr = stderr.expect("stderr result");
    Ok(BoundedOutput {
        status,
        stdout: stdout.bytes,
        truncated: stdout.truncated || stderr.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        git_diff_args, git_status_args, git_untracked_args, parse_status, HANDOFF_PATHSPEC,
        HANDOFF_SUBTREE_PATHSPEC,
    };

    #[test]
    fn parses_nul_safe_status_paths() {
        let changes = parse_status(b" M src/main.rs\0?? new file.txt\0");
        assert_eq!(changes[0].path, "src/main.rs");
        assert_eq!(changes[1].status, "??");
    }

    #[test]
    fn untracked_probe_is_path_scoped() {
        let args = git_untracked_args("new file.txt");
        assert_eq!(
            args[0..5],
            [
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--"
            ]
        );
        assert_eq!(args[5], "new file.txt");
    }

    #[test]
    fn status_excludes_the_entire_handoff_subtree() {
        let args = git_status_args();
        assert_eq!(
            &args[args.len() - 2..],
            [
                HANDOFF_PATHSPEC.to_string(),
                HANDOFF_SUBTREE_PATHSPEC.to_string()
            ]
        );
    }

    #[test]
    fn diff_disables_helpers_and_excludes_the_entire_handoff_subtree() {
        let args = git_diff_args("HEAD", None);
        assert!(args.contains(&"--no-ext-diff".to_string()));
        assert!(args.contains(&"--no-textconv".to_string()));
        assert_eq!(
            &args[args.len() - 2..],
            [
                HANDOFF_PATHSPEC.to_string(),
                HANDOFF_SUBTREE_PATHSPEC.to_string()
            ]
        );
    }

    #[test]
    fn path_specific_diff_keeps_the_path_and_handoff_exclusions() {
        let args = git_diff_args("HEAD", Some("src/main.rs"));
        assert_eq!(
            &args[args.len() - 3..],
            [
                "src/main.rs".to_string(),
                HANDOFF_PATHSPEC.to_string(),
                HANDOFF_SUBTREE_PATHSPEC.to_string()
            ]
        );
    }
}
